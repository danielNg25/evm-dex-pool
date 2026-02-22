use crate::PoolRegistry;
use alloy::providers::Provider;
use anyhow::Result;
use log::{debug, error, info};
use std::sync::Arc;
use tokio::sync::mpsc;

use super::block_source::{
    BlockSource, EventBatch, LatestBlockSource, PendingBlockSource, ProcessingMode,
    WebsocketBlockSource,
};
use super::event_processor::{EventProcessor, PendingEvent};
use super::metrics::CollectorMetrics;
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
pub struct UnifiedPoolUpdater {
    source: Box<dyn BlockSource>,
    event_processor: EventProcessor,
    pool_registry: Arc<PoolRegistry>,
}

impl UnifiedPoolUpdater {
    pub fn new<P: Provider + Send + Sync + 'static>(
        provider: Arc<P>,
        pool_registry: Arc<PoolRegistry>,
        metrics: Option<Arc<dyn CollectorMetrics>>,
        swap_event_tx: Option<mpsc::Sender<PendingEvent>>,
        start_block: u64,
        max_blocks_per_batch: u64,
        mode: UpdaterMode,
    ) -> Self {
        // Initialize last_processed_block
        let current_block = pool_registry.get_last_processed_block();
        if current_block == 0 {
            pool_registry.set_last_processed_block(start_block);
            info!("Initialized last processed block to {}", start_block);
        } else if start_block > 0 && start_block > current_block {
            pool_registry.set_last_processed_block(start_block);
            info!(
                "Updated last processed block from {} to {}",
                current_block, start_block
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
                provider,
                Arc::clone(&pool_registry),
                Arc::clone(&topics),
                max_blocks_per_batch,
            )),
            UpdaterMode::LatestBlock { wait_time_ms } => Box::new(LatestBlockSource::new(
                provider,
                Arc::clone(&pool_registry),
                Arc::clone(&topics),
                max_blocks_per_batch,
                wait_time_ms,
            )),
            UpdaterMode::Websocket { event_queue } => Box::new(WebsocketBlockSource::new(
                provider,
                event_queue,
                Arc::clone(&pool_registry),
                Arc::clone(&topics),
                max_blocks_per_batch,
            )),
        };

        Self {
            source,
            event_processor,
            pool_registry,
        }
    }

    /// Run the updater loop. This never returns under normal operation.
    pub async fn start(&mut self) -> Result<()> {
        info!("UnifiedPoolUpdater: starting bootstrap...");
        self.source.bootstrap().await?;
        info!("UnifiedPoolUpdater: bootstrap complete, entering main loop");

        loop {
            debug!("UnifiedPoolUpdater: calling next_batch...");
            let batch = self.source.next_batch().await;

            match batch {
                Ok(EventBatch {
                    events,
                    processing_mode,
                    processed_through_block,
                }) => {
                    let event_count = events.len();
                    debug!(
                        "UnifiedPoolUpdater: received batch with {} events",
                        event_count
                    );

                    match processing_mode {
                        ProcessingMode::ApplyOnly => {
                            debug!("UnifiedPoolUpdater: applying {} events (ApplyOnly)", event_count);
                            self.event_processor.apply_events_to_registry(&events).await;
                        }
                        ProcessingMode::ConfirmedWithSwaps => {
                            debug!(
                                "UnifiedPoolUpdater: processing {} events (ConfirmedWithSwaps)",
                                event_count
                            );
                            self.event_processor
                                .process_confirmed_events(events)
                                .await;
                        }
                        ProcessingMode::Pending => {
                            debug!("UnifiedPoolUpdater: processing {} events (Pending)", event_count);
                            self.event_processor.process_pending_events(events).await;
                        }
                    }

                    if let Some(block) = processed_through_block {
                        self.pool_registry.set_last_processed_block(block);
                        info!("Successfully processed through block {}", block);
                    }
                    debug!("UnifiedPoolUpdater: batch processing complete");
                }
                Err(e) => {
                    error!("Error fetching event batch: {}", e);
                }
            }
        }
    }
}
