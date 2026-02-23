use alloy::primitives::Address;
use std::collections::HashMap;

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
}

/// Configuration for batch pool fetching from RPC.
pub struct PoolFetchConfig {
    pub multicall_address: Address,
    pub chain_id: u64,
    /// Factory address (lowercase hex) -> fee in 1_000_000 basis. Used for V2 pools.
    pub factory_to_fee: HashMap<String, u64>,
    /// Aerodrome-style factory addresses (for stable/volatile detection).
    pub aero_factory_addresses: Vec<Address>,
    /// Number of pools to fetch in parallel per chunk (default: 10).
    pub chunk_size: usize,
    /// Milliseconds to wait between chunks (rate limiting).
    pub wait_time_between_chunks: u64,
    /// Max retry attempts per pool with exponential backoff (default: 5).
    pub max_retries: u32,
}
