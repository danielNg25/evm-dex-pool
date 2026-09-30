use alloy::primitives::Address;
use std::collections::HashMap;
use std::time::Duration;

/// [`CollectorConfig::ws_block_settle_ms`] when unset: enough to take in a
/// block whose logs arrive as one burst (under ~1 ms apart), which the
/// websocket source used to cut mid-burst -- most of run 11's 17% split
/// blocks (Avalanche, 2026-09-29).
pub const DEFAULT_WS_BLOCK_SETTLE_MS: u64 = 10;

/// Configuration for the collector bootstrap.
///
/// Decoupled from any application-specific config so every project
/// can map its own config into this struct.
pub struct CollectorConfig {
    pub start_block: u64,
    pub max_blocks_per_batch: u64,
    pub use_pending_blocks: bool,
    pub use_websocket: bool,
    pub websocket_urls: Vec<String>,
    /// Polling interval in milliseconds for LatestBlock mode.
    pub wait_time: u64,
    /// Run the background fee reader (`collector::fee_reader`): pools whose
    /// fee no event announces -- Algebra with `DYNAMIC_FEE` on, Ramses-family
    /// pools answering `currentFee()` -- are re-read off the hot path, after a
    /// batch that touched them and every 30 s. Reads nothing on a chain with
    /// no such pools. The name predates the reader; it is kept so existing
    /// configs still load.
    pub refetch_algebra_fee: bool,
    /// Websocket mode: how long a block's logs must be quiet before the block
    /// is handed on, whole, for pricing (a later block starting hands it on
    /// at once). `None` = [`DEFAULT_WS_BLOCK_SETTLE_MS`].
    ///
    /// Set it per chain and endpoint to just above the endpoint's gap between
    /// the stages of one block's logs. `api.avax.network` publishes a busy
    /// block's logs in stages ~190 ms apart (2026-09-30: 187-196 ms, in 3-20%
    /// of blocks depending on load), so it needs ~210 ms there; a block handed
    /// on at its first stage is priced on half a block. A log that still
    /// arrives after its block went on is applied late and logged, never lost.
    pub ws_block_settle_ms: Option<u64>,
}

impl CollectorConfig {
    /// [`Self::ws_block_settle_ms`], or its default.
    pub fn ws_block_settle(&self) -> Duration {
        Duration::from_millis(
            self.ws_block_settle_ms
                .unwrap_or(DEFAULT_WS_BLOCK_SETTLE_MS),
        )
    }
}

/// Configuration for batch pool fetching from RPC.
pub struct PoolFetchConfig {
    /// Multicall3 address override. If None, falls back to chain-specific
    /// custom address, then standard MULTICALL3 (0xcA11bde05977b3631167028862bE2a173976CA11).
    pub multicall_address: Option<Address>,
    pub chain_id: u64,
    /// Factory address (hex, any case) -> fee in 1_000_000 basis. Used for V2 pools.
    pub factory_to_fee: HashMap<String, u64>,
    /// Aerodrome-style factory addresses (for stable/volatile detection).
    pub aero_factory_addresses: Vec<Address>,
    /// Number of pools to fetch in parallel per chunk (default: 10).
    pub chunk_size: usize,
    /// Milliseconds to wait between chunks (rate limiting).
    pub wait_time_between_chunks: u64,
    /// Max retry attempts per pool with exponential backoff (default: 5).
    pub max_retries: u32,
    /// Whether to fetch pools within each chunk in parallel (default: true).
    /// Set to false for rate-limited RPCs to fetch pools sequentially.
    pub parallel_fetch: bool,
}
