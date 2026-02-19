use alloy::primitives::Address;
use dashmap::DashMap;
use crate::{PoolInterface, PoolType, Topic};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Optimized pool registry using DashMap for lock-free concurrent reads.
///
/// Key optimizations over the original:
/// - R1: `DashMap` for `by_address` instead of `Arc<RwLock<HashMap>>` - shard-level locking
/// - R2: Removed `by_type` index - computed on-demand (only used during init, not hot paths)
/// - R3: `AtomicU64` for `last_processed_block` instead of `Arc<RwLock<u64>>`
/// - R4: Plain `RwLock` for topics (set once, then read-only)
/// - R5: Convenience methods delegate to `get_pools_by_type`
pub struct PoolRegistry {
    by_address: Arc<DashMap<Address, Arc<tokio::sync::RwLock<Box<dyn PoolInterface + Send + Sync>>>>>,
    last_processed_block: AtomicU64,
    topics: RwLock<Vec<Topic>>,
    profitable_topics: RwLock<HashSet<Topic>>,
    network_id: u64,
}

impl PoolRegistry {
    pub fn new(network_id: u64) -> Self {
        Self {
            by_address: Arc::new(DashMap::new()),
            last_processed_block: AtomicU64::new(0),
            topics: RwLock::new(Vec::new()),
            profitable_topics: RwLock::new(HashSet::new()),
            network_id,
        }
    }

    /// Set network ID for this registry
    pub fn set_network_id(&mut self, network_id: u64) {
        self.network_id = network_id;
    }

    /// Get network ID
    pub fn get_network_id(&self) -> u64 {
        self.network_id
    }

    /// Get total pool count (lock-free)
    pub fn pool_count(&self) -> usize {
        self.by_address.len()
    }

    /// Add a pool to the registry
    pub fn add_pool(&self, pool: Box<dyn PoolInterface + Send + Sync>) {
        let address = pool.address();
        self.by_address.insert(address, Arc::new(tokio::sync::RwLock::new(pool)));
    }

    /// Get a pool by address (lock-free read)
    pub fn get_pool(
        &self,
        address: &Address,
    ) -> Option<Arc<tokio::sync::RwLock<Box<dyn PoolInterface + Send + Sync>>>> {
        self.by_address.get(address).map(|entry| Arc::clone(entry.value()))
    }

    /// Remove a pool by address
    pub fn remove_pool(
        &self,
        address: &Address,
    ) -> Option<Arc<tokio::sync::RwLock<Box<dyn PoolInterface + Send + Sync>>>> {
        self.by_address.remove(address).map(|(_, pool)| pool)
    }

    /// Get all pools
    pub fn get_all_pools(&self) -> Vec<Arc<tokio::sync::RwLock<Box<dyn PoolInterface + Send + Sync>>>> {
        self.by_address
            .iter()
            .map(|entry| Arc::clone(entry.value()))
            .collect()
    }

    /// Get pools by type (computed on-demand, R2)
    pub fn get_pools_by_type(
        &self,
        pool_type: PoolType,
    ) -> Vec<Arc<tokio::sync::RwLock<Box<dyn PoolInterface + Send + Sync>>>> {
        self.by_address
            .iter()
            .filter(|entry| {
                entry.value().try_read().map(|p| p.pool_type() == pool_type).unwrap_or(false)
            })
            .map(|entry| Arc::clone(entry.value()))
            .collect()
    }

    /// Get V2 pools (R5: delegates to get_pools_by_type)
    pub fn get_v2_pools(&self) -> Vec<Arc<tokio::sync::RwLock<Box<dyn PoolInterface + Send + Sync>>>> {
        self.get_pools_by_type(PoolType::UniswapV2)
    }

    /// Get V3 pools (R5: delegates to get_pools_by_type)
    pub fn get_v3_pools(&self) -> Vec<Arc<tokio::sync::RwLock<Box<dyn PoolInterface + Send + Sync>>>> {
        self.get_pools_by_type(PoolType::UniswapV3)
    }

    /// Get addresses by pool type (computed on-demand)
    pub fn get_addresses_by_type(&self, pool_type: PoolType) -> Vec<Address> {
        self.by_address
            .iter()
            .filter(|entry| {
                entry.value().try_read().map(|p| p.pool_type() == pool_type).unwrap_or(false)
            })
            .map(|entry| *entry.key())
            .collect()
    }

    /// Get V2 addresses
    pub fn get_v2_addresses(&self) -> Vec<Address> {
        self.get_addresses_by_type(PoolType::UniswapV2)
    }

    /// Get V3 addresses
    pub fn get_v3_addresses(&self) -> Vec<Address> {
        self.get_addresses_by_type(PoolType::UniswapV3)
    }

    /// Get all addresses (lock-free)
    pub fn get_all_addresses(&self) -> Vec<Address> {
        self.by_address.iter().map(|entry| *entry.key()).collect()
    }

    /// Log summary of all pools
    pub async fn log_summary(&self) -> String {
        let mut summary = String::new();
        summary.push_str("Pool Registry Summary:\n");
        summary.push_str("--------------------------------\n");

        for entry in self.by_address.iter() {
            let pool = entry.value().read().await;
            summary.push_str(&format!("Pool: {}\n", pool.log_summary()));
        }
        summary
    }

    /// Get the last processed block (R3: lock-free atomic)
    pub fn get_last_processed_block(&self) -> u64 {
        self.last_processed_block.load(Ordering::Relaxed)
    }

    /// Set the last processed block (R3: lock-free atomic)
    pub fn set_last_processed_block(&self, block_number: u64) {
        self.last_processed_block.store(block_number, Ordering::Relaxed);
    }

    /// Add topics to the registry
    pub fn add_topics(&self, topics: Vec<Topic>) {
        let mut topics_lock = self.topics.write().unwrap();
        topics_lock.extend(topics);
    }

    /// Add profitable topics to the registry
    pub fn add_profitable_topics(&self, topics: Vec<Topic>) {
        let mut topics_lock = self.profitable_topics.write().unwrap();
        topics_lock.extend(topics);
    }

    /// Get all topics
    pub fn get_topics(&self) -> Vec<Topic> {
        self.topics.read().unwrap().clone()
    }

    /// Get profitable topics
    pub fn get_profitable_topics(&self) -> HashSet<Topic> {
        self.profitable_topics.read().unwrap().clone()
    }
}

impl Clone for PoolRegistry {
    fn clone(&self) -> Self {
        Self {
            by_address: Arc::clone(&self.by_address),
            last_processed_block: AtomicU64::new(self.last_processed_block.load(Ordering::Relaxed)),
            topics: RwLock::new(self.topics.read().unwrap().clone()),
            profitable_topics: RwLock::new(self.profitable_topics.read().unwrap().clone()),
            network_id: self.network_id,
        }
    }
}

impl Default for PoolRegistry {
    fn default() -> Self {
        Self::new(0)
    }
}

impl std::fmt::Debug for PoolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolRegistry")
            .field("pool_count", &self.by_address.len())
            .field("last_processed_block", &self.get_last_processed_block())
            .field("network_id", &self.network_id)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockPool;

    #[tokio::test]
    async fn test_add_and_get_pool() {
        let registry = PoolRegistry::new(1);
        let pool = MockPool::new_boxed();
        let addr = pool.address();
        registry.add_pool(pool);

        assert_eq!(registry.pool_count(), 1);
        let pool = registry.get_pool(&addr);
        assert!(pool.is_some());
    }

    #[tokio::test]
    async fn test_remove_pool() {
        let registry = PoolRegistry::new(1);
        let pool = MockPool::new_boxed();
        let addr = pool.address();
        registry.add_pool(pool);

        let removed = registry.remove_pool(&addr);
        assert!(removed.is_some());
        assert_eq!(registry.pool_count(), 0);
        assert!(registry.get_pool(&addr).is_none());
    }

    #[tokio::test]
    async fn test_last_processed_block() {
        let registry = PoolRegistry::new(1);
        assert_eq!(registry.get_last_processed_block(), 0);
        registry.set_last_processed_block(42);
        assert_eq!(registry.get_last_processed_block(), 42);
    }

    #[tokio::test]
    async fn test_topics() {
        let registry = PoolRegistry::new(1);
        let topics = vec![[0u8; 32].into()];
        registry.add_topics(topics.clone());
        assert_eq!(registry.get_topics().len(), 1);
    }

    #[tokio::test]
    async fn test_clone_shares_state() {
        let registry = PoolRegistry::new(1);
        let pool = MockPool::new_boxed();
        let addr = pool.address();
        registry.add_pool(pool);

        let cloned = registry.clone();
        // Cloned registry shares the by_address map
        assert_eq!(cloned.pool_count(), 1);
        assert!(cloned.get_pool(&addr).is_some());
    }

    #[tokio::test]
    async fn test_get_all_addresses() {
        let registry = PoolRegistry::new(1);
        let pool = MockPool::new_boxed();
        let addr = pool.address();
        registry.add_pool(pool);

        let addresses = registry.get_all_addresses();
        assert_eq!(addresses.len(), 1);
        assert!(addresses.contains(&addr));
    }
}
