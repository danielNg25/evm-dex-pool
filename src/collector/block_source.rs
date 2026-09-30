use crate::PoolRegistry;
use crate::PoolType;
use crate::Topic;
use alloy::eips::BlockNumberOrTag;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::Log;
use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, warn};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::event_processor::fetch_events_with_retry;
use super::{enrich_log_timestamps_where, fetch_events, EventQueue};

/// Enrich the LB pools' logs in `events` with block timestamps, taking any
/// time already `known` first. Only LB pools read a log's block time (the
/// variable-fee clock), so every other pool's logs are left alone: a header
/// fetched for a block holding none of them was a round trip for nothing, and
/// Sentio, which zeroes every log's timestamp, made that every block.
pub(crate) async fn enrich_if_lb_pools_present<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pool_registry: &PoolRegistry,
    events: &mut [Log],
    known: &HashMap<u64, u64>,
) -> Result<()> {
    let lb: HashSet<Address> = pool_registry
        .get_addresses_by_type(PoolType::TraderJoeLB)
        .into_iter()
        .collect();
    if lb.is_empty() {
        return Ok(());
    }
    enrich_log_timestamps_where(
        provider,
        events,
        &|log: &Log| lb.contains(&log.address()),
        known,
    )
    .await
}

/// Position of a log in chain order: `(block number, transaction index, log index)`.
///
/// Ordered so that Rust's lexicographic tuple comparison *is* chain order.
type LogPosition = (u64, u64, u64);

/// Chain-order position of `log`, or `None` when any of the three coordinates
/// is missing.
///
/// Every log returned by `eth_getLogs` over a numbered block range is mined and
/// carries all three; `None` means a malformed or non-conforming RPC response.
fn log_position(log: &Log) -> Option<LogPosition> {
    Some((log.block_number?, log.transaction_index?, log.log_index?))
}

/// Whether `position` is at or after `boundary` in chain order.
///
/// The three coordinates must be compared *lexicographically* — block first,
/// and the lower coordinates only to break a tie on the higher ones. Comparing
/// them independently (`block >= b && tx >= t && log >= l`) misses any later
/// block whose transaction index happens to be smaller: `(101, 2, 0)` is
/// strictly after `(100, 5, 3)` in chain order, yet `2 >= 5` is false. During
/// websocket bootstrap that means RPC catch-up applies an event the queue also
/// delivers, so the event lands twice and reserves are over-stated.
fn is_at_or_after(position: LogPosition, boundary: LogPosition) -> bool {
    position >= boundary
}

/// Inclusive `(from, to)` block ranges the websocket catch-up has to fetch.
///
/// Covers `(last_processed_block, first_event_block]`: the cursor block's events
/// are already reflected in registry state, and `first_event_block` *is*
/// included because the first queued event sits mid-block — the events before
/// it in that block reach the registry only through this catch-up.
///
/// `fetch_events` builds a filter that is inclusive at both ends, so successive
/// ranges must not share a block: `[a, b]` is followed by `[b + 1, ..]`, never
/// `[b, ..]`. Sharing it applied every batch boundary block's events twice.
fn catchup_batches(
    last_processed_block: u64,
    max_blocks_per_batch: u64,
    first_event_block: u64,
) -> Vec<(u64, u64)> {
    // A zero-configured batch size would otherwise underflow below and yield a
    // range that never advances.
    let span = max_blocks_per_batch.max(1);
    let mut batches = Vec::new();
    let mut start = last_processed_block.saturating_add(1);
    while start <= first_event_block {
        let end = std::cmp::min(start.saturating_add(span - 1), first_event_block);
        batches.push((start, end));
        if end >= first_event_block {
            break;
        }
        start = end + 1;
    }
    batches
}

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
    ConfirmedBatches {
        current_block: u64,
        latest_block: u64,
    },
    /// Fetch and process pending block
    FetchPending,
}

pub struct PendingBlockSource<P: Provider + Send + Sync + 'static> {
    provider: Arc<P>,
    pool_registry: Arc<PoolRegistry>,
    topics: Arc<Vec<Topic>>,
    max_blocks_per_batch: u64,
    phase: PendingPhase,
    chain_id: u64,
}

impl<P: Provider + Send + Sync + 'static> PendingBlockSource<P> {
    pub fn new(
        provider: Arc<P>,
        pool_registry: Arc<PoolRegistry>,
        topics: Arc<Vec<Topic>>,
        max_blocks_per_batch: u64,
    ) -> Self {
        let chain_id = pool_registry.get_network_id();
        Self {
            provider,
            pool_registry,
            topics,
            max_blocks_per_batch,
            phase: PendingPhase::PollBlockNumber,
            chain_id,
        }
    }
}

#[async_trait]
impl<P: Provider + Send + Sync + 'static> BlockSource for PendingBlockSource<P> {
    async fn next_batch(&mut self) -> Result<EventBatch> {
        loop {
            match &mut self.phase {
                PendingPhase::PollBlockNumber => {
                    let latest_block =
                        get_block_number_with_retry(&self.provider, self.chain_id).await;
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

                    debug!(
                        "[Chain {}] Processing blocks: {} - {}",
                        self.chain_id, from, batch_end
                    );

                    let mut events = fetch_events_with_retry(
                        &self.provider,
                        self.pool_registry.get_all_addresses(),
                        self.topics.to_vec(),
                        BlockNumberOrTag::Number(from),
                        BlockNumberOrTag::Number(batch_end),
                        self.chain_id,
                    )
                    .await?;

                    debug!(
                        "[Chain {}] Processing {} events from {} to {}",
                        self.chain_id,
                        events.len(),
                        from,
                        batch_end
                    );

                    enrich_if_lb_pools_present(
                        &self.provider,
                        &self.pool_registry,
                        &mut events,
                        &HashMap::new(),
                    )
                    .await?;

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

                    let mut events = fetch_events_with_retry(
                        &self.provider,
                        addresses,
                        self.topics.to_vec(),
                        BlockNumberOrTag::Pending,
                        BlockNumberOrTag::Pending,
                        self.chain_id,
                    )
                    .await?;

                    debug!(
                        "[Chain {}] Processing {} events from pending block",
                        self.chain_id,
                        events.len()
                    );

                    enrich_if_lb_pools_present(
                        &self.provider,
                        &self.pool_registry,
                        &mut events,
                        &HashMap::new(),
                    )
                    .await?;

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
    /// Block times the poll has seen, for the LB timestamp enrichment.
    known_block_times: HashMap<u64, u64>,
    chain_id: u64,
}

impl<P: Provider + Send + Sync + 'static> LatestBlockSource<P> {
    pub fn new(
        provider: Arc<P>,
        pool_registry: Arc<PoolRegistry>,
        topics: Arc<Vec<Topic>>,
        max_blocks_per_batch: u64,
        wait_time_ms: u64,
    ) -> Self {
        let chain_id = pool_registry.get_network_id();
        Self {
            provider,
            pool_registry,
            topics,
            max_blocks_per_batch,
            wait_time_ms,
            batches: Vec::new(),
            known_block_times: HashMap::new(),
            chain_id,
        }
    }
}

#[async_trait]
impl<P: Provider + Send + Sync + 'static> BlockSource for LatestBlockSource<P> {
    async fn next_batch(&mut self) -> Result<EventBatch> {
        loop {
            // If we have pre-computed batches, yield the next one
            if let Some((from, to, is_latest_single)) = self.batches.pop() {
                debug!(
                    "[Chain {}] Fetching events for blocks {} - {} (remaining batches: {})",
                    self.chain_id,
                    from,
                    to,
                    self.batches.len()
                );

                let mut events = fetch_events_with_retry(
                    &self.provider,
                    self.pool_registry.get_all_addresses(),
                    self.topics.to_vec(),
                    BlockNumberOrTag::Number(from),
                    BlockNumberOrTag::Number(to),
                    self.chain_id,
                )
                .await?;

                enrich_if_lb_pools_present(
                    &self.provider,
                    &self.pool_registry,
                    &mut events,
                    &self.known_block_times,
                )
                .await?;

                let mode = if is_latest_single {
                    ProcessingMode::ConfirmedWithSwaps
                } else {
                    ProcessingMode::ApplyOnly
                };

                debug!(
                    "[Chain {}] Fetched {} events from {} to {} (mode: {})",
                    self.chain_id,
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
                "[Chain {}] No batches, sleeping {}ms before polling...",
                self.chain_id, self.wait_time_ms
            );
            tokio::time::sleep(Duration::from_millis(self.wait_time_ms)).await;

            debug!("[Chain {}] Calling get_latest_header...", self.chain_id);
            let (latest_block, latest_time) =
                get_latest_header_with_retry(&self.provider, self.chain_id).await;
            self.known_block_times.insert(latest_block, latest_time);
            self.known_block_times
                .retain(|&n, _| n + KNOWN_BLOCK_TIMES_KEPT > latest_block);
            let last_processed = self.pool_registry.get_last_processed_block();
            let next_block = last_processed + 1;
            debug!(
                "[Chain {}] Block number received: latest={}, last_processed={}, next={}",
                self.chain_id, latest_block, last_processed, next_block
            );

            if next_block > latest_block {
                debug!(
                    "[Chain {}] Waiting for new blocks. Last processed: {}, Latest: {}",
                    self.chain_id, last_processed, latest_block
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
            debug!(
                "[Chain {}] Created {} batches for blocks {} to {}",
                self.chain_id,
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

/// How long a block's logs must be quiet before the websocket source hands the
/// block on. An Avalanche node publishes a busy block's logs in stages: in a
/// 5-minute probe (2026-09-30) the second chunk followed the first after
/// 41-67 ms through publicnode and ~190 ms through api.avax.network, and an
/// HTTP `eth_getLogs` for the block was no authority either (it answered short
/// in 4 of 84 blocks). A block handed on at its first chunk is priced on half
/// a block: run 11 split 17% of blocks that way, and their opportunities
/// replayed 80.7% exact against 99.3% for whole blocks. With several websocket
/// feeds what counts is the gap between the first copy of each chunk, which
/// the feed that delivered chunk 1 first bounds; a chunk that still comes
/// after the window is applied late (and logged), never lost.
pub const WS_BLOCK_SETTLE: Duration = Duration::from_millis(75);

/// A block the websocket source is ready to hand on.
#[derive(Debug)]
struct Settled {
    block: u64,
    logs: Vec<Log>,
    /// The block was handed on before, and these logs came after.
    late: bool,
    /// Never below a block already handed on.
    processed_through: u64,
}

/// Holds websocket logs per block until the block is complete: nothing new for
/// it for `settle`, or a later block has started -- a feed delivers all of a
/// block's chunks before the next block's first.
#[derive(Debug)]
struct BlockSettler {
    settle: Duration,
    /// Logs per block, with the arrival time of the block's latest log.
    pending: BTreeMap<u64, (Vec<Log>, Instant)>,
    released_through: u64,
}

impl BlockSettler {
    fn new(settle: Duration, released_through: u64) -> Self {
        Self {
            settle,
            pending: BTreeMap::new(),
            released_through,
        }
    }

    fn add(&mut self, logs: Vec<Log>, now: Instant) {
        // A reorg's retraction of a log: the pools apply state-setting events
        // outright, so replaying a removed one would only corrupt them.
        for log in logs.into_iter().filter(|log| !log.removed) {
            // A mined log always carries its block; one that does not is
            // filed with the last block handed on, so it goes out as a late
            // batch rather than being lost.
            let block = log.block_number.unwrap_or(self.released_through);
            let entry = self
                .pending
                .entry(block)
                .or_insert_with(|| (Vec::new(), now));
            entry.0.push(log);
            entry.1 = now;
        }
    }

    /// The oldest pending block, if it is complete.
    fn take_ready(&mut self, now: Instant) -> Option<Settled> {
        let (&block, (_, last_arrival)) = self.pending.iter().next()?;
        let later_block_started = self.pending.len() > 1;
        if !later_block_started && now.duration_since(*last_arrival) < self.settle {
            return None;
        }
        let (mut logs, _) = self.pending.remove(&block)?;
        // Arrival order is chain order only while every feed is ordered and
        // loses nothing; a whole block in hand can be put back in chain
        // order, which matters because Swap and Sync set state outright.
        logs.sort_by_key(|log| {
            (
                log.transaction_index.unwrap_or(u64::MAX),
                log.log_index.unwrap_or(u64::MAX),
            )
        });
        let late = block <= self.released_through;
        self.released_through = self.released_through.max(block);
        Some(Settled {
            block,
            logs,
            late,
            processed_through: self.released_through,
        })
    }

    /// When the oldest pending block settles, if nothing more arrives for it.
    fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .values()
            .next()
            .map(|(_, last_arrival)| *last_arrival + self.settle)
    }
}

pub struct WebsocketBlockSource<P: Provider + Send + Sync + 'static> {
    provider: Arc<P>,
    event_queue: EventQueue,
    pool_registry: Arc<PoolRegistry>,
    topics: Arc<Vec<Topic>>,
    max_blocks_per_batch: u64,
    chain_id: u64,
    settler: BlockSettler,
}

impl<P: Provider + Send + Sync + 'static> WebsocketBlockSource<P> {
    pub fn new(
        provider: Arc<P>,
        event_queue: EventQueue,
        pool_registry: Arc<PoolRegistry>,
        topics: Arc<Vec<Topic>>,
        max_blocks_per_batch: u64,
    ) -> Self {
        let chain_id = pool_registry.get_network_id();
        Self {
            provider,
            event_queue,
            pool_registry,
            topics,
            max_blocks_per_batch,
            chain_id,
            settler: BlockSettler::new(WS_BLOCK_SETTLE, 0),
        }
    }
}

#[async_trait]
impl<P: Provider + Send + Sync + 'static> BlockSource for WebsocketBlockSource<P> {
    async fn bootstrap(&mut self) -> Result<()> {
        let latest_block = self.provider.get_block_number().await?;
        info!("[Chain {}] Latest block: {}", self.chain_id, latest_block);

        let mut events = self.event_queue.get_all_available_events().await;
        info!(
            "[Chain {}] Found {} events in EventQueue",
            self.chain_id,
            events.len()
        );

        // Chain-order position of the earliest queued event. RPC catch-up must
        // cover exactly `(last_processed, first_queued)` — everything from this
        // position onwards arrives from the queue below, so applying any of it
        // here too would double-apply it.
        //
        // `min` rather than `first` because it does not assume the queue
        // preserves chain order (several websocket listeners may feed it), and
        // it ignores any log whose payload left the position incomplete.
        // Falling back to `(latest_block, 0, 0)` when the queue is empty keeps
        // the previous behaviour: catch up through `latest_block` exclusive,
        // leaving that block's events to the live subscription.
        let first_queued_position: LogPosition = events
            .iter()
            .filter_map(log_position)
            .min()
            .unwrap_or((latest_block, 0, 0));
        let (first_event_block, first_event_index, first_event_log_index) = first_queued_position;
        info!(
            "[Chain {}] First event block: {}; tx index: {}; log index: {}",
            self.chain_id, first_event_block, first_event_index, first_event_log_index
        );

        // Catch up from last processed block to first websocket event.
        //
        // `last_processed_block` is the block whose events the registry state
        // already includes, so catch-up starts at the block *after* it — the
        // same convention `LatestBlockSource`, `PendingBlockSource` and
        // `catchup_registry_to_block` use. Starting at `last_processed_block`
        // re-applied that whole block.
        let last_processed_block = self.pool_registry.get_last_processed_block();
        let topics = self.topics.to_vec();

        info!(
            "[Chain {}] Catching up to first event block {}",
            self.chain_id, first_event_block
        );
        // Batches tile the gap exactly once each — see `catchup_batches`.
        // Indexed rather than iterated so the error arm below can retry the
        // same batch, as it always has, instead of skipping it.
        let batches = catchup_batches(
            last_processed_block,
            self.max_blocks_per_batch,
            first_event_block,
        );
        let mut batch_index = 0usize;
        while batch_index < batches.len() {
            let (start_block, end_block) = batches[batch_index];

            match fetch_events(
                &self.provider,
                self.pool_registry.get_all_addresses(),
                topics.clone(),
                BlockNumberOrTag::Number(start_block),
                BlockNumberOrTag::Number(end_block),
            )
            .await
            {
                Ok(mut fetched_events) => {
                    info!(
                        "[Chain {}] Fetched {} events in batch {} - {}",
                        self.chain_id,
                        fetched_events.len(),
                        start_block,
                        end_block
                    );

                    enrich_if_lb_pools_present(
                        &self.provider,
                        &self.pool_registry,
                        &mut fetched_events,
                        &self.event_queue.known_block_times(),
                    )
                    .await?;

                    let mut should_break = false;
                    for event in fetched_events {
                        // A log the RPC returned without a full position cannot
                        // be ordered against the queue boundary. Skip it rather
                        // than panic on the unwrap or risk applying an event the
                        // queue also delivers; every mined log carries all three
                        // coordinates, so this is a malformed response.
                        let Some(position) = log_position(&event) else {
                            error!(
                                "[Chain {}] Skipping log without a chain position (block {:?}, tx index {:?}, log index {:?}) for pool {}",
                                self.chain_id,
                                event.block_number,
                                event.transaction_index,
                                event.log_index,
                                event.address()
                            );
                            continue;
                        };

                        // Stop if we've reached the first WS event. This event
                        // and everything after it arrive from the queue below.
                        if is_at_or_after(position, first_queued_position) {
                            info!(
                                "[Chain {}] Reached first event {:?} block {} tx index {} log index {}, breaking",
                                self.chain_id,
                                event.transaction_hash,
                                position.0,
                                position.1,
                                position.2
                            );
                            should_break = true;
                            break;
                        }

                        if let Some(pool) = self.pool_registry.get_pool(&event.address()) {
                            if let Err(e) = pool.write().await.apply_log(&event) {
                                error!(
                                    "[Chain {}] Error applying event {} for pool {}, event {:?}",
                                    self.chain_id,
                                    e,
                                    event.address(),
                                    event.transaction_hash
                                );
                            }
                        }
                    }

                    // The queue takes over from here; the remaining batches
                    // would only re-apply what it already holds.
                    if should_break {
                        break;
                    }
                }
                Err(e) => {
                    error!(
                        "[Chain {}] Error fetching events in batch {}-{}: {}",
                        self.chain_id, start_block, end_block, e
                    );
                    continue;
                }
            }

            batch_index += 1;
        }

        // Apply the initial websocket events that were buffered
        enrich_if_lb_pools_present(
            &self.provider,
            &self.pool_registry,
            &mut events,
            &self.event_queue.known_block_times(),
        )
        .await?;
        let max_ws_block = events.iter().filter_map(|e| e.block_number).max();
        for event in events {
            if let Some(pool) = self.pool_registry.get_pool(&event.address()) {
                if let Err(e) = pool.write().await.apply_log(&event) {
                    error!(
                        "[Chain {}] Error applying event {} for pool {}, event {:?}",
                        self.chain_id,
                        e,
                        event.address(),
                        event.transaction_hash
                    );
                }
            }
        }

        // Advance cursor so it reflects the actual chain state after catch-up.
        // Without this, last_processed_block stays at the stale bootstrap value
        // and add_pools may try to fetch at a block where new pools don't exist.
        let bootstrap_block = max_ws_block.unwrap_or(first_event_block);
        self.pool_registry.set_last_processed_block(bootstrap_block);
        // Logs still arriving for a block bootstrap already applied are late.
        self.settler = BlockSettler::new(WS_BLOCK_SETTLE, bootstrap_block);
        info!(
            "[Chain {}] Bootstrap complete, set last_processed_block to {}",
            self.chain_id, bootstrap_block
        );

        Ok(())
    }

    async fn next_batch(&mut self) -> Result<EventBatch> {
        loop {
            let arrived = self.event_queue.get_all_available_events().await;
            if !arrived.is_empty() {
                self.settler.add(arrived, Instant::now());
            }

            // One whole block per batch, once it is complete (see
            // `WS_BLOCK_SETTLE`), so every swap in it is priced on the
            // block's final state.
            if let Some(settled) = self.settler.take_ready(Instant::now()) {
                if settled.late {
                    warn!(
                        "[Chain {}] Websocket: {} log(s) for block {} arrived after the block \
                         was handed on; applying them now",
                        self.chain_id,
                        settled.logs.len(),
                        settled.block
                    );
                }
                let mut events = settled.logs;
                debug!(
                    "[Chain {}] Processing {} events of block {} from EventQueue",
                    self.chain_id,
                    events.len(),
                    settled.block
                );

                // The listeners' `newHeads` subscriptions have usually recorded
                // these blocks' times already; a block that is not there yet
                // (its logs beat its header, or a reconnect gap) is fetched as
                // before.
                enrich_if_lb_pools_present(
                    &self.provider,
                    &self.pool_registry,
                    &mut events,
                    &self.event_queue.known_block_times(),
                )
                .await?;

                // Logs of a block older than one already handed on land on a
                // newer state (Swap and Sync set it outright), so they are
                // applied but never priced; a late chunk of the newest block
                // is still in chain order and is priced as usual.
                let processing_mode = if settled.block < settled.processed_through {
                    ProcessingMode::ApplyOnly
                } else {
                    ProcessingMode::ConfirmedWithSwaps
                };
                return Ok(EventBatch {
                    events,
                    processing_mode,
                    processed_through_block: Some(settled.processed_through),
                });
            }

            // Check the queue at least every 5 ms, and wake for the pending
            // block's deadline if that comes sooner.
            let poll = Duration::from_millis(5);
            let wait = self
                .settler
                .next_deadline()
                .map(|d| d.saturating_duration_since(Instant::now()).min(poll))
                .unwrap_or(poll)
                .max(Duration::from_millis(1));
            tokio::time::sleep(wait).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Retry an RPC read until it answers: a 30-second timeout per attempt to
/// handle stale RPC connections, and exponential backoff from 50 ms to 500 ms
/// between attempts. `what` names the read in the logs. An answer with nothing
/// in it (`Ok(None)`) is retried like an error.
async fn rpc_with_retry<T, E, F, Fut>(chain_id: u64, what: &str, mut read: F) -> T
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<T>, E>>,
{
    let mut backoff = Duration::from_millis(50);
    let max_backoff = Duration::from_millis(500);
    let rpc_timeout = Duration::from_secs(30);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        if attempt > 1 {
            debug!("[Chain {}] {} attempt {}", chain_id, what, attempt);
        }
        match tokio::time::timeout(rpc_timeout, read()).await {
            Ok(Ok(Some(value))) => return value,
            Ok(Ok(None)) => error!(
                "[Chain {}] No result {} (attempt {}), retrying in {}ms",
                chain_id,
                what,
                attempt,
                backoff.as_millis()
            ),
            Ok(Err(e)) => error!(
                "[Chain {}] Error {} (attempt {}), retrying in {}ms: {}",
                chain_id,
                what,
                attempt,
                backoff.as_millis(),
                e
            ),
            Err(_) => error!(
                "[Chain {}] Timeout {} (attempt {}, {}s), retrying in {}ms",
                chain_id,
                what,
                attempt,
                rpc_timeout.as_secs(),
                backoff.as_millis()
            ),
        }
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff * 2, max_backoff);
    }
}

/// Get block number from provider with exponential backoff retry.
async fn get_block_number_with_retry<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    chain_id: u64,
) -> u64 {
    rpc_with_retry(chain_id, "fetching block number", || async move {
        provider.get_block_number().await.map(Some)
    })
    .await
}

/// Block times kept from the poll.
const KNOWN_BLOCK_TIMES_KEPT: u64 = 64;

/// The latest block's number and timestamp. Polling the header instead of the
/// bare number costs the same round trip and hands the LB enrichment the time
/// of the block it is about to fetch, so a steady-state single-block batch
/// needs no header fetch of its own.
async fn get_latest_header_with_retry<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    chain_id: u64,
) -> (u64, u64) {
    rpc_with_retry(chain_id, "fetching latest block", || async move {
        provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .map(|block| block.map(|b| (b.header.number, b.header.timestamp)))
    })
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The stop test as it was written before the fix: three independent `>=`
    /// comparisons ANDed together. Kept here only to pin the difference.
    fn legacy_stop_test(position: LogPosition, boundary: LogPosition) -> bool {
        position.0 >= boundary.0 && position.1 >= boundary.1 && position.2 >= boundary.2
    }

    fn log_at(block: u64, tx_index: u64, log_index: u64) -> Log {
        Log {
            block_number: Some(block),
            transaction_index: Some(tx_index),
            log_index: Some(log_index),
            ..Default::default()
        }
    }

    // -- websocket block settling ---------------------------------------------

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A block goes on only once nothing new has arrived for it for the settle
    /// window.
    #[test]
    fn a_block_is_held_until_its_logs_go_quiet() {
        let t = Instant::now();
        let mut s = BlockSettler::new(ms(75), 99);
        s.add(vec![log_at(100, 0, 0)], t);
        assert!(s.take_ready(t + ms(74)).is_none());
        let settled = s.take_ready(t + ms(75)).unwrap();
        assert_eq!((settled.block, settled.logs.len()), (100, 1));
        assert!(!settled.late);
        assert_eq!(settled.processed_through, 100);
    }

    /// The node's second chunk of a busy block, arriving inside the window,
    /// goes out with the first -- the split this exists to prevent.
    #[test]
    fn a_second_chunk_inside_the_window_joins_its_block() {
        let t = Instant::now();
        let mut s = BlockSettler::new(ms(75), 99);
        s.add(vec![log_at(100, 0, 0), log_at(100, 1, 1)], t);
        s.add(vec![log_at(100, 9, 5)], t + ms(50));
        assert!(s.take_ready(t + ms(100)).is_none(), "quiet since +50 only");
        assert_eq!(s.take_ready(t + ms(125)).unwrap().logs.len(), 3);
    }

    /// A feed delivers a block's chunks before the next block's, so a later
    /// block starting completes the earlier one without waiting.
    #[test]
    fn a_later_block_releases_the_earlier_one_at_once() {
        let t = Instant::now();
        let mut s = BlockSettler::new(ms(75), 99);
        s.add(vec![log_at(100, 0, 0)], t);
        s.add(vec![log_at(101, 0, 0)], t + ms(10));
        assert_eq!(s.take_ready(t + ms(10)).unwrap().block, 100);
        assert!(s.take_ready(t + ms(10)).is_none(), "101 is not quiet yet");
        assert_eq!(s.take_ready(t + ms(85)).unwrap().block, 101);
    }

    /// A log for a block already handed on is still applied, flagged late, and
    /// never moves the processed cursor back.
    #[test]
    fn a_log_after_its_block_went_on_is_applied_late_and_the_cursor_holds() {
        let t = Instant::now();
        let mut s = BlockSettler::new(ms(75), 99);
        s.add(vec![log_at(101, 0, 0)], t);
        assert_eq!(s.take_ready(t + ms(75)).unwrap().processed_through, 101);

        s.add(vec![log_at(101, 7, 3)], t + ms(200));
        let late = s.take_ready(t + ms(275)).unwrap();
        assert!(late.late);
        assert_eq!((late.block, late.processed_through), (101, 101));

        s.add(vec![log_at(100, 0, 0)], t + ms(300));
        let older = s.take_ready(t + ms(375)).unwrap();
        assert!(older.late);
        assert_eq!((older.block, older.processed_through), (100, 101));
    }

    /// The cursor goes only to the block handed on, never to a newer block
    /// still held: the fee reader and add_pools read at it.
    #[test]
    fn the_cursor_stops_at_the_block_handed_on() {
        let t = Instant::now();
        let mut s = BlockSettler::new(ms(75), 99);
        s.add(vec![log_at(100, 0, 0), log_at(101, 0, 0)], t);
        let first = s.take_ready(t).unwrap();
        assert_eq!((first.block, first.processed_through), (100, 100));
    }

    /// A late log for an older block, arriving while a newer block settles,
    /// goes out at once (a later block has started) and leaves the newer
    /// block's timer alone.
    #[test]
    fn a_late_older_log_goes_out_without_touching_the_settling_block() {
        let t = Instant::now();
        let mut s = BlockSettler::new(ms(75), 99);
        s.add(vec![log_at(100, 0, 0)], t);
        assert_eq!(s.take_ready(t + ms(75)).unwrap().block, 100);
        s.add(vec![log_at(101, 0, 0)], t + ms(1000));
        s.add(vec![log_at(100, 3, 1)], t + ms(1030));
        let late = s.take_ready(t + ms(1030)).unwrap();
        assert!(late.late);
        assert_eq!((late.block, late.processed_through), (100, 100));
        assert!(
            s.take_ready(t + ms(1074)).is_none(),
            "101 settles 75 ms after its own log"
        );
        assert_eq!(s.take_ready(t + ms(1075)).unwrap().block, 101);
    }

    /// A settled block goes out in chain order whatever order its logs came.
    #[test]
    fn a_settled_block_is_in_chain_order() {
        let t = Instant::now();
        let mut s = BlockSettler::new(ms(75), 99);
        s.add(
            vec![log_at(100, 9, 5), log_at(100, 2, 1), log_at(100, 9, 4)],
            t,
        );
        let order: Vec<(u64, u64)> = s
            .take_ready(t + ms(75))
            .unwrap()
            .logs
            .iter()
            .map(|l| (l.transaction_index.unwrap(), l.log_index.unwrap()))
            .collect();
        assert_eq!(order, vec![(2, 1), (9, 4), (9, 5)]);
    }

    /// A reorg's removed log is never applied.
    #[test]
    fn a_removed_log_is_dropped() {
        let t = Instant::now();
        let mut s = BlockSettler::new(ms(75), 99);
        let mut removed = log_at(100, 1, 1);
        removed.removed = true;
        s.add(vec![log_at(100, 0, 0), removed], t);
        assert_eq!(s.take_ready(t + ms(75)).unwrap().logs.len(), 1);
    }

    /// Logs for a block bootstrap already applied count as late.
    #[test]
    fn a_log_for_the_bootstrap_block_is_late() {
        let t = Instant::now();
        let mut s = BlockSettler::new(ms(75), 100);
        s.add(vec![log_at(100, 4, 2)], t);
        assert!(s.take_ready(t + ms(75)).unwrap().late);
    }

    // -- ordering test ------------------------------------------------------

    #[test]
    fn later_block_with_smaller_tx_index_is_at_or_after() {
        // The live Avalanche counterexample: the first queued event is at
        // (100, 5, 3) and a catch-up event at (101, 2, 0) is strictly later in
        // chain order, so catch-up must stop rather than apply it — the queue
        // delivers it.
        let boundary = (100, 5, 3);
        assert!(is_at_or_after((101, 2, 0), boundary));
        // ...which is exactly where the old three-way AND went wrong.
        assert!(!legacy_stop_test((101, 2, 0), boundary));
    }

    #[test]
    fn same_block_with_smaller_log_index_is_before() {
        let boundary = (100, 5, 3);
        assert!(!is_at_or_after((100, 5, 2), boundary));
        assert!(!is_at_or_after((100, 4, 9), boundary));
        assert!(!is_at_or_after((99, 9, 9), boundary));
    }

    #[test]
    fn boundary_itself_is_at_or_after() {
        let boundary = (100, 5, 3);
        assert!(is_at_or_after(boundary, boundary));
        assert!(is_at_or_after((100, 5, 4), boundary));
        assert!(is_at_or_after((100, 6, 0), boundary));
    }

    #[test]
    fn log_position_requires_all_three_coordinates() {
        assert_eq!(log_position(&log_at(100, 5, 3)), Some((100, 5, 3)));

        let mut no_block = log_at(100, 5, 3);
        no_block.block_number = None;
        assert_eq!(log_position(&no_block), None);

        let mut no_tx = log_at(100, 5, 3);
        no_tx.transaction_index = None;
        assert_eq!(log_position(&no_tx), None);

        let mut no_log_index = log_at(100, 5, 3);
        no_log_index.log_index = None;
        assert_eq!(log_position(&no_log_index), None);
    }

    // -- batch boundary -----------------------------------------------------

    /// Every block in `(last_processed, first_event_block]` appears in exactly
    /// one batch — no repeats (which double-apply) and no gaps (which drop).
    fn assert_tiles_exactly_once(last_processed: u64, span: u64, first_event_block: u64) {
        let batches = catchup_batches(last_processed, span, first_event_block);
        let mut expected = last_processed + 1;
        for (from, to) in &batches {
            assert_eq!(
                *from,
                expected,
                "batch {:?} does not resume at {} (batches: {:?})",
                (from, to),
                expected,
                batches
            );
            assert!(to >= from, "empty batch {:?}", (from, to));
            expected = to + 1;
        }
        assert_eq!(
            expected,
            first_event_block + 1,
            "batches stop short of {} (batches: {:?})",
            first_event_block,
            batches
        );
    }

    #[test]
    fn batches_do_not_share_a_boundary_block() {
        // fetch_events is inclusive at both ends, so [101,110] must be followed
        // by [111,...]. The pre-fix loop produced [100,110], [110,120], ...
        assert_eq!(
            catchup_batches(100, 10, 135),
            vec![(101, 110), (111, 120), (121, 130), (131, 135)]
        );
    }

    #[test]
    fn batches_start_after_the_processed_cursor() {
        // last_processed's events are already in registry state.
        assert_eq!(catchup_batches(100, 10, 105), vec![(101, 105)]);
    }

    #[test]
    fn batches_cover_every_block_exactly_once() {
        for span in [1, 2, 3, 7, 10, 1000] {
            for first_event_block in 100..=140 {
                assert_tiles_exactly_once(100, span, first_event_block);
            }
        }
    }

    #[test]
    fn adjacent_and_equal_cursors_terminate() {
        // First queued event is in the very next block: that block still needs
        // fetching, because events preceding the queued one are not in the queue.
        assert_eq!(catchup_batches(100, 10, 101), vec![(101, 101)]);
        // Nothing to catch up: the cursor is already at or past the queue.
        assert_eq!(catchup_batches(100, 10, 100), vec![]);
        assert_eq!(catchup_batches(100, 10, 99), vec![]);
    }

    #[test]
    fn zero_batch_size_still_advances() {
        assert_eq!(
            catchup_batches(100, 0, 103),
            vec![(101, 101), (102, 102), (103, 103)]
        );
    }

    #[test]
    fn saturating_bounds_do_not_overflow() {
        assert_eq!(
            catchup_batches(u64::MAX - 1, 10, u64::MAX),
            vec![(u64::MAX, u64::MAX)]
        );
        assert_eq!(
            catchup_batches(u64::MAX - 2, u64::MAX, u64::MAX),
            vec![(u64::MAX - 1, u64::MAX)]
        );
    }
}
