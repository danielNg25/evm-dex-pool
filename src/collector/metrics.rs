use alloy::primitives::TxHash;

/// Trait for recording collector-level metrics.
///
/// Consumers implement this for their own metrics infrastructure.
/// Pass as `Option<Arc<dyn CollectorMetrics>>` to `EventProcessor` / `UnifiedPoolUpdater`.
pub trait CollectorMetrics: Send + Sync {
    /// Record that a new opportunity was received from a swap event.
    fn add_opportunity(&self, tx_hash: TxHash, log_index: u64, received_at: u64);

    /// Record the timestamp at which the event was processed.
    fn set_processed_at(&self, tx_hash: TxHash, log_index: u64, processed_at: u64);
}
