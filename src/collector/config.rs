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
