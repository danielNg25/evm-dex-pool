//! Periodic fee refetch for Algebra V3 pools.
//!
//! Some Algebra V3 deployments use a dynamic fee plugin that updates the swap
//! fee every block via an external mechanism (e.g. a price oracle), and emit no
//! event the collector consumes, so the cached fee drifts unless it is read back.
//!
//! Ramses-family CL pools are NOT handled here. Their fee also moves, but every
//! change emits `FeeAdjustment(oldFee, newFee)`, which `UniswapV3Pool::apply_log`
//! applies in log order. That is strictly better than this poll: the poll runs
//! after a batch's swaps have been simulated, so it prices a block on the
//! previous block's fee -- the cause of 5 of 9 failures in run 6's replay.
//!
//! When `CollectorConfig::refetch_algebra_fee` is enabled, the collector calls
//! [`refetch_dynamic_fees`] after each event batch is applied. It does a single
//! multicall (chunked) to read `fee()` for every tracked address and writes the
//! fresh value back into the registry.
//!
//! `fee()` is read through the Uniswap V3 ABI, which types it `uint24`. Algebra
//! declares it `uint16`; both ABI-encode to a 32-byte word, so the wider type
//! decodes an Algebra response correctly, while the narrower one would fail to
//! decode a Ramses fee above 65535 (e.g. the 100000 = 10% tier).

use crate::contracts_rpc::RpcIUniswapV3Pool as IUniswapV3Pool;
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

/// Refetch `fee()` for every tracked mutable-fee pool and update the registry.
///
/// Uses `try_aggregate(false)` so a single misbehaving pool does not fail
/// the whole batch. Per-pool failures are logged at `warn` level and the
/// stale fee is retained for that pool until the next refetch.
pub async fn refetch_dynamic_fees<P: Provider + Send + Sync>(
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
        "[Chain {}] Refetching dynamic fees for {} pool(s)",
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
            let instance = IUniswapV3Pool::new(addr, provider);
            mc = mc.add_dynamic(instance.fee());
        }

        let results = mc.try_aggregate(false).await?;

        for (i, &addr) in chunk.iter().enumerate() {
            let new_fee: U24 = match &results[i] {
                Ok(fee) => *fee,
                Err(e) => {
                    warn!(
                        "[Chain {}] dynamic fee() failed for {}: {}",
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
                v3.set_fee(new_fee);
                updated += 1;
                info!(
                    "[Chain {}] dynamic fee updated: {} {} -> {}",
                    chain_id, addr, old_fee, new_fee
                );
            }
        }
    }

    info!(
        "[Chain {}] dynamic fee refetch complete at block {}: {} updated, {} failed",
        chain_id, block, updated, failed
    );
    Ok(())
}
