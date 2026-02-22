use crate::PoolRegistry;
use crate::Topic;
use alloy::eips::BlockNumberOrTag;
use alloy::providers::Provider;
use alloy::rpc::types::Log;
use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info};
use std::sync::Arc;
use std::time::Duration;

use super::event_processor::fetch_events_with_retry;
use super::{fetch_events, EventQueue};

/// How the unified updater should process a batch of events.
pub enum ProcessingMode {
    /// Apply to pool registry only. Used for catch-up / non-latest confirmed blocks.
    ApplyOnly,
    /// Apply to registry and send swap events to simulator. Used for live confirmed events.
    ConfirmedWithSwaps,
    /// Clone pools speculatively and send swap events. Used for pending block.
    Pending,
}

/// A batch of events yielded by a BlockSource.
pub struct EventBatch {
    pub events: Vec<Log>,
    pub processing_mode: ProcessingMode,
    /// Block number to record as last_processed after successful processing.
    /// None for pending block events (they don't advance the confirmed cursor).
    pub processed_through_block: Option<u64>,
}

/// Strategy trait for sourcing blockchain events.
///
/// Each implementation handles a different way of receiving events
/// (RPC polling, pending block speculation, websocket streaming).
#[async_trait]
pub trait BlockSource: Send {
    /// One-time setup (e.g. catching up from last processed block to live state).
    async fn bootstrap(&mut self) -> Result<()> {
        Ok(())
    }

    /// Yield the next batch of events to process.
    /// Implementations may block/sleep internally while waiting for new data.
    async fn next_batch(&mut self) -> Result<EventBatch>;
}

// ---------------------------------------------------------------------------
// PendingBlockSource — polls RPC, processes confirmed blocks, then pending block
// ---------------------------------------------------------------------------

enum PendingPhase {
    /// Need to fetch latest block number from RPC
    PollBlockNumber,
    /// Processing confirmed block batches
    ConfirmedBatches { current_block: u64, latest_block: u64 },
    /// Fetch and process pending block
    FetchPending,
}

pub struct PendingBlockSource<P: Provider + Send + Sync + 'static> {
    provider: Arc<P>,
    pool_registry: Arc<PoolRegistry>,
    topics: Arc<Vec<Topic>>,
    max_blocks_per_batch: u64,
    phase: PendingPhase,
}

impl<P: Provider + Send + Sync + 'static> PendingBlockSource<P> {
    pub fn new(
        provider: Arc<P>,
        pool_registry: Arc<PoolRegistry>,
        topics: Arc<Vec<Topic>>,
        max_blocks_per_batch: u64,
    ) -> Self {
        Self {
            provider,
            pool_registry,
            topics,
            max_blocks_per_batch,
            phase: PendingPhase::PollBlockNumber,
        }
    }
}

#[async_trait]
impl<P: Provider + Send + Sync + 'static> BlockSource for PendingBlockSource<P> {
    async fn next_batch(&mut self) -> Result<EventBatch> {
        loop {
            match &mut self.phase {
                PendingPhase::PollBlockNumber => {
                    let latest_block = get_block_number_with_retry(&self.provider).await;
                    let last_processed = self.pool_registry.get_last_processed_block();
                    let next_block = last_processed + 1;

                    if next_block > latest_block {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }

                    self.phase = PendingPhase::ConfirmedBatches {
                        current_block: next_block,
                        latest_block,
                    };
                }

                PendingPhase::ConfirmedBatches {
                    current_block,
                    latest_block,
                } => {
                    let batch_end = std::cmp::min(
                        *current_block + self.max_blocks_per_batch - 1,
                        *latest_block,
                    );
                    let from = *current_block;
                    let latest = *latest_block;

                    info!("Processing blocks: {} - {}", from, batch_end);

                    let events = fetch_events_with_retry(
                        &self.provider,
                        self.pool_registry.get_all_addresses(),
                        self.topics.to_vec(),
                        BlockNumberOrTag::Number(from),
                        BlockNumberOrTag::Number(batch_end),
                    )
                    .await?;

                    info!(
                        "Processing {} events from {} to {}",
                        events.len(),
                        from,
                        batch_end
                    );

                    // Advance phase
                    let next_block = batch_end + 1;
                    if next_block > latest {
                        self.phase = PendingPhase::FetchPending;
                    } else {
                        self.phase = PendingPhase::ConfirmedBatches {
                            current_block: next_block,
                            latest_block: latest,
                        };
                    }

                    return Ok(EventBatch {
                        events,
                        processing_mode: ProcessingMode::ApplyOnly,
                        processed_through_block: Some(batch_end),
                    });
                }

                PendingPhase::FetchPending => {
                    let addresses = self.pool_registry.get_all_addresses();
                    if addresses.is_empty() {
                        self.phase = PendingPhase::PollBlockNumber;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }

                    let events = fetch_events_with_retry(
                        &self.provider,
                        addresses,
                        self.topics.to_vec(),
                        BlockNumberOrTag::Pending,
                        BlockNumberOrTag::Pending,
                    )
                    .await?;

                    info!("Processing {} events from pending block", events.len());

                    // Reset to poll for next iteration
                    self.phase = PendingPhase::PollBlockNumber;

                    return Ok(EventBatch {
                        events,
                        processing_mode: ProcessingMode::Pending,
                        processed_through_block: None,
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LatestBlockSource — polls RPC, sends swap events only for latest single block
// ---------------------------------------------------------------------------

pub struct LatestBlockSource<P: Provider + Send + Sync + 'static> {
    provider: Arc<P>,
    pool_registry: Arc<PoolRegistry>,
    topics: Arc<Vec<Topic>>,
    max_blocks_per_batch: u64,
    wait_time_ms: u64,
    /// Internal state: remaining confirmed batches to process
    batches: Vec<(u64, u64, bool)>, // (from, to, is_latest_single_block)
}

impl<P: Provider + Send + Sync + 'static> LatestBlockSource<P> {
    pub fn new(
        provider: Arc<P>,
        pool_registry: Arc<PoolRegistry>,
        topics: Arc<Vec<Topic>>,
        max_blocks_per_batch: u64,
        wait_time_ms: u64,
    ) -> Self {
        Self {
            provider,
            pool_registry,
            topics,
            max_blocks_per_batch,
            wait_time_ms,
            batches: Vec::new(),
        }
    }
}

#[async_trait]
impl<P: Provider + Send + Sync + 'static> BlockSource for LatestBlockSource<P> {
    async fn next_batch(&mut self) -> Result<EventBatch> {
        loop {
            // If we have pre-computed batches, yield the next one
            if let Some((from, to, is_latest_single)) = self.batches.pop() {
                info!(
                    "Fetching events for blocks {} - {} (remaining batches: {})",
                    from,
                    to,
                    self.batches.len()
                );

                let events = fetch_events_with_retry(
                    &self.provider,
                    self.pool_registry.get_all_addresses(),
                    self.topics.to_vec(),
                    BlockNumberOrTag::Number(from),
                    BlockNumberOrTag::Number(to),
                )
                .await?;

                let mode = if is_latest_single {
                    ProcessingMode::ConfirmedWithSwaps
                } else {
                    ProcessingMode::ApplyOnly
                };

                info!(
                    "Fetched {} events from {} to {} (mode: {})",
                    events.len(),
                    from,
                    to,
                    match &mode {
                        ProcessingMode::ApplyOnly => "ApplyOnly",
                        ProcessingMode::ConfirmedWithSwaps => "ConfirmedWithSwaps",
                        ProcessingMode::Pending => "Pending",
                    }
                );

                return Ok(EventBatch {
                    events,
                    processing_mode: mode,
                    processed_through_block: Some(to),
                });
            }

            // No batches left — sleep before polling to avoid RPC rate limiting
            debug!(
                "No batches, sleeping {}ms before polling...",
                self.wait_time_ms
            );
            tokio::time::sleep(Duration::from_millis(self.wait_time_ms)).await;

            debug!("Calling get_block_number...");
            let latest_block = get_block_number_with_retry(&self.provider).await;
            let last_processed = self.pool_registry.get_last_processed_block();
            let next_block = last_processed + 1;
            debug!(
                "Block number received: latest={}, last_processed={}, next={}",
                latest_block, last_processed, next_block
            );

            if next_block > latest_block {
                info!(
                    "Waiting for new blocks. Last processed: {}, Latest: {}",
                    last_processed, latest_block
                );
                continue;
            }

            // Compute batch ranges (stored in reverse so we can pop from front)
            let mut batches = Vec::new();
            let mut current = next_block;
            while current <= latest_block {
                let batch_end =
                    std::cmp::min(current + self.max_blocks_per_batch - 1, latest_block);
                // Only the last batch that covers a single block emits swap events
                let is_latest_single = batch_end == latest_block && current == batch_end;
                batches.push((current, batch_end, is_latest_single));
                current = batch_end + 1;
            }
            info!(
                "Created {} batches for blocks {} to {}",
                batches.len(),
                next_block,
                latest_block
            );
            // Reverse so pop() gives us the first batch
            batches.reverse();
            self.batches = batches;
        }
    }
}

// ---------------------------------------------------------------------------
// WebsocketBlockSource — bootstraps via RPC, then streams from EventQueue
// ---------------------------------------------------------------------------

pub struct WebsocketBlockSource<P: Provider + Send + Sync + 'static> {
    provider: Arc<P>,
    event_queue: EventQueue,
    pool_registry: Arc<PoolRegistry>,
    topics: Arc<Vec<Topic>>,
    max_blocks_per_batch: u64,
}

impl<P: Provider + Send + Sync + 'static> WebsocketBlockSource<P> {
    pub fn new(
        provider: Arc<P>,
        event_queue: EventQueue,
        pool_registry: Arc<PoolRegistry>,
        topics: Arc<Vec<Topic>>,
        max_blocks_per_batch: u64,
    ) -> Self {
        Self {
            provider,
            event_queue,
            pool_registry,
            topics,
            max_blocks_per_batch,
        }
    }
}

#[async_trait]
impl<P: Provider + Send + Sync + 'static> BlockSource for WebsocketBlockSource<P> {
    async fn bootstrap(&mut self) -> Result<()> {
        let latest_block = self.provider.get_block_number().await?;
        info!("Latest block: {}", latest_block);

        let events = self.event_queue.get_all_available_events().await;
        info!("Found {} events in EventQueue", events.len());

        let mut first_event_block = latest_block;
        let mut first_event_index = 0;
        let mut first_event_log_index = 0;
        if !events.is_empty() {
            let first_event = events.first().unwrap();
            first_event_block = first_event.block_number.unwrap();
            first_event_index = first_event.transaction_index.unwrap();
            first_event_log_index = first_event.log_index.unwrap();
        }
        info!(
            "First event block: {}; index: {}",
            first_event_block, first_event_index
        );

        // Catch up from last processed block to first websocket event
        let last_processed_block = self.pool_registry.get_last_processed_block();
        let mut start_block = last_processed_block;
        let topics = self.topics.to_vec();

        info!("Catching up to first event block {}", first_event_block);
        while start_block < first_event_block {
            let end_block =
                std::cmp::min(start_block + self.max_blocks_per_batch, first_event_block);

            match fetch_events(
                &self.provider,
                self.pool_registry.get_all_addresses(),
                topics.clone(),
                BlockNumberOrTag::Number(start_block),
                BlockNumberOrTag::Number(end_block),
            )
            .await
            {
                Ok(fetched_events) => {
                    info!(
                        "Fetched {} events in batch {} - {}",
                        fetched_events.len(),
                        start_block,
                        end_block
                    );

                    let mut should_break = false;
                    for event in fetched_events {
                        // Stop if we've reached the first WS event
                        if event.block_number.unwrap() >= first_event_block
                            && event.transaction_index.unwrap() >= first_event_index
                            && event.log_index.unwrap() >= first_event_log_index
                        {
                            info!(
                                "Reached first event {} block {} index {}, breaking",
                                event.transaction_hash.unwrap(),
                                event.block_number.unwrap(),
                                event.transaction_index.unwrap()
                            );
                            should_break = true;
                            break;
                        }

                        if let Some(pool) = self.pool_registry.get_pool(&event.address()) {
                            if let Err(e) = pool.write().await.apply_log(&event) {
                                error!(
                                    "Error applying event {} for pool {}, event {}",
                                    e,
                                    event.address(),
                                    event.transaction_hash.unwrap()
                                );
                            }
                        }
                    }

                    if end_block >= first_event_block || should_break {
                        break;
                    }
                }
                Err(e) => {
                    error!(
                        "Error fetching events in batch {}-{}: {}",
                        start_block, end_block, e
                    );
                    continue;
                }
            }

            start_block = end_block;
        }

        // Apply the initial websocket events that were buffered
        for event in events {
            if let Some(pool) = self.pool_registry.get_pool(&event.address()) {
                if let Err(e) = pool.write().await.apply_log(&event) {
                    error!(
                        "Error applying event {} for pool {}, event {}",
                        e,
                        event.address(),
                        event.transaction_hash.unwrap()
                    );
                }
            }
        }

        Ok(())
    }

    async fn next_batch(&mut self) -> Result<EventBatch> {
        loop {
            let events = self.event_queue.get_all_available_events().await;
            if events.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }

            info!("Processing {} events from EventQueue", events.len());

            return Ok(EventBatch {
                events,
                processing_mode: ProcessingMode::ConfirmedWithSwaps,
                processed_through_block: None,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Get block number from provider with exponential backoff retry.
/// Includes a 30-second timeout per attempt to handle stale RPC connections.
async fn get_block_number_with_retry<P: Provider + Send + Sync>(provider: &Arc<P>) -> u64 {
    let mut backoff = Duration::from_millis(50);
    let max_backoff = Duration::from_millis(500);
    let rpc_timeout = Duration::from_secs(30);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        if attempt > 1 {
            debug!("get_block_number attempt {}", attempt);
        }
        match tokio::time::timeout(rpc_timeout, provider.get_block_number()).await {
            Ok(Ok(block)) => return block,
            Ok(Err(e)) => {
                error!(
                    "Error fetching block number (attempt {}), retrying in {}ms: {}",
                    attempt,
                    backoff.as_millis(),
                    e
                );
            }
            Err(_) => {
                error!(
                    "Timeout fetching block number (attempt {}, {}s), retrying in {}ms",
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
