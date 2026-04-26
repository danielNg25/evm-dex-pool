//! Periodic fee refetch for Algebra V3 pools.
//!
//! Some Algebra V3 deployments use a dynamic fee plugin that updates the swap
//! fee every block via an external mechanism (e.g. a price oracle). This fee
//! change does not emit any on-chain event the collector listens to, so the
//! cached fee in the registry drifts from the on-chain fee over time.
//!
//! When `CollectorConfig::refetch_algebra_fee` is enabled, the collector calls
//! [`refetch_algebra_v3_fees`] after each event batch is applied. It does a
//! single multicall (chunked) to read `fee()` for every tracked Algebra V3
//! pool address and writes the fresh value back into the registry.

use crate::contracts_rpc::RpcAlgebraV3Pool as AlgebraV3Pool;
use crate::v3::UniswapV3Pool;
use crate::PoolRegistry;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{aliases::U24, Address};
use alloy::providers::{MulticallBuilder, Provider};
use anyhow::Result;
use log::{info, warn};
use std::sync::Arc;

/// Maximum number of `fee()` calls bundled into a single multicall.
const REFETCH_CHUNK_SIZE: usize = 250;

/// Refetch `fee()` for every tracked Algebra V3 pool and update the registry.
///
/// Uses `try_aggregate(false)` so a single misbehaving pool does not fail
/// the whole batch. Per-pool failures are logged at `warn` level and the
/// stale fee is retained for that pool until the next refetch.
pub async fn refetch_algebra_v3_fees<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    registry: &Arc<PoolRegistry>,
    addresses: &[Address],
    multicall_address: Address,
    chain_id: u64,
    block: u64,
) -> Result<()> {
    if addresses.is_empty() {
        return Ok(());
    }

    info!(
        "[Chain {}] Refetching Algebra V3 fees for {} pool(s)",
        chain_id,
        addresses.len()
    );

    let mut updated = 0usize;
    let mut failed = 0usize;
    for chunk in addresses.chunks(REFETCH_CHUNK_SIZE) {
        let mut mc = MulticallBuilder::new_dynamic(provider)
            .address(multicall_address)
            .block(BlockId::Number(BlockNumberOrTag::Number(block)));
        for &addr in chunk {
            let instance = AlgebraV3Pool::new(addr, provider);
            mc = mc.add_dynamic(instance.fee());
        }

        let results = mc.try_aggregate(false).await?;

        for (i, &addr) in chunk.iter().enumerate() {
            let new_fee: u16 = match &results[i] {
                Ok(fee) => *fee,
                Err(e) => {
                    warn!(
                        "[Chain {}] Algebra V3 fee() failed for {}: {}",
                        chain_id, addr, e
                    );
                    failed += 1;
                    continue;
                }
            };

            let Some(pool_arc) = registry.get_pool(&addr) else {
                continue;
            };
            let mut pool = pool_arc.write().await;
            if let Some(v3) = pool.as_any_mut().downcast_mut::<UniswapV3Pool>() {
                let old_fee = v3.fee;
                v3.set_fee(U24::from(new_fee));
                updated += 1;
                info!(
                    "[Chain {}] Algebra V3 fee updated: {} {} -> {}",
                    chain_id, addr, old_fee, new_fee
                );
            }
        }
    }

    info!(
        "[Chain {}] Algebra V3 fee refetch complete at block {}: {} updated, {} failed",
        chain_id, block, updated, failed
    );
    Ok(())
}
