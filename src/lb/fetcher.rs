//! RPC-based fetcher for TraderJoe Liquidity Book pools.

use crate::contracts_rpc::RpcILBPair as ILBPair;
use crate::lb::LBPool;
use crate::TokenInfo;
use alloy::eips::BlockId;
use alloy::primitives::aliases::U24;
use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::providers::{MulticallBuilder, Provider};
use anyhow::Result;
use log::info;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Base storage slot of `_tree` in standard LBPair v2.1.
///
/// LBPair storage layout: LBToken (slots 0-2), `_parameters`=3, `_reserves`=4,
/// `_protocolFees`=5, `_bins`=6, `_tree`=7 (level0), 8 (level1 mapping), 9 (level2 mapping).
const LB_TREE_BASE_SLOT: u64 = 7;

/// Compute the storage slot for a Solidity mapping entry: `keccak256(key ++ base_slot)`.
fn mapping_storage_slot(key: B256, base_slot: U256) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(key.as_slice());
    buf[32..].copy_from_slice(&base_slot.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

/// Collect indices of all set bits in a U256 value.
fn set_bits(word: U256) -> Vec<u16> {
    let mut bits = Vec::new();
    for b in 0..=255u16 {
        if word & (U256::from(1) << b) != U256::ZERO {
            bits.push(b);
        }
    }
    bits
}

/// Discover all non-empty bin IDs by reading the 3-level TreeMath bitmap from storage.
///
/// The LBPair `_tree` (TreeUint24) is a private struct with:
/// - `level0` (bytes32): root bitmap — 256 bits, one per level1 group
/// - `level1` (mapping(bytes32 => bytes32)): 256 bits per entry
/// - `level2` (mapping(bytes32 => bytes32)): 256 bits per entry (leaf level)
///
/// Bin ID = `(level0_bit << 16) | (level1_bit << 8) | level2_bit`
async fn discover_bins_from_tree<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pool_address: Address,
    block_number: BlockId,
) -> Result<Vec<u32>> {
    let base = LB_TREE_BASE_SLOT;

    // Step 1: Read level0 (1 RPC call)
    let level0: U256 = provider
        .get_storage_at(pool_address, U256::from(base))
        .block_id(block_number)
        .await?;

    if level0.is_zero() {
        return Ok(vec![]);
    }

    let level0_bits = set_bits(level0);

    // Step 2: Read all populated level1 entries concurrently
    let level1_futures: Vec<_> = level0_bits
        .iter()
        .map(|&k| {
            let slot = mapping_storage_slot(B256::from(U256::from(k)), U256::from(base + 1));
            let provider = provider.clone();
            async move {
                provider
                    .get_storage_at(pool_address, slot)
                    .block_id(block_number)
                    .await
            }
        })
        .collect();
    let level1_values = futures_util::future::try_join_all(level1_futures).await?;

    // Step 3: Build level2 keys from level1 set bits, read concurrently
    let mut level2_keys: Vec<u16> = Vec::new();
    for (i, &k1) in level0_bits.iter().enumerate() {
        for bit in set_bits(level1_values[i]) {
            level2_keys.push((k1 << 8) | bit);
        }
    }

    let level2_futures: Vec<_> = level2_keys
        .iter()
        .map(|&k| {
            let slot = mapping_storage_slot(B256::from(U256::from(k)), U256::from(base + 2));
            let provider = provider.clone();
            async move {
                provider
                    .get_storage_at(pool_address, slot)
                    .block_id(block_number)
                    .await
            }
        })
        .collect();
    let level2_values = futures_util::future::try_join_all(level2_futures).await?;

    // Step 4: Extract all non-empty bin IDs from level2 bitmaps
    let mut bin_ids: Vec<u32> = Vec::new();
    for (i, &k2) in level2_keys.iter().enumerate() {
        for bit in set_bits(level2_values[i]) {
            bin_ids.push((k2 as u32) << 8 | bit as u32);
        }
    }

    Ok(bin_ids)
}

/// Fetch a TraderJoe Liquidity Book pool from chain.
///
/// Discovers all non-empty bins via storage bitmap reads of the on-chain TreeMath structure.
/// Falls back to a fixed window (`±bin_depth`) around `active_id` if storage reads fail.
pub async fn fetch_lb_pool<P: Provider + Send + Sync, T: TokenInfo>(
    provider: &Arc<P>,
    pool_address: Address,
    block_number: BlockId,
    token_info: &T,
    multicall_address: Address,
    chain_id: u64,
    bin_depth: Option<u32>,
) -> Result<LBPool> {
    info!(
        "[Chain {}] Fetching LB pool: {}",
        chain_id, pool_address
    );

    let lb_instance = ILBPair::new(pool_address, provider);

    // ── Batch 1: Pool metadata + fee parameters ─────────────────────────
    let multicall_result = provider
        .multicall()
        .address(multicall_address)
        .add(lb_instance.getTokenX()) // 0
        .add(lb_instance.getTokenY()) // 1
        .add(lb_instance.getBinStep()) // 2
        .add(lb_instance.getActiveId()) // 3
        .add(lb_instance.getStaticFeeParameters()) // 4
        .add(lb_instance.getVariableFeeParameters()) // 5
        .block(block_number)
        .try_aggregate(false)
        .await?;

    let token_x_raw = multicall_result.0?;
    let token_y_raw = multicall_result.1?;
    let bin_step = multicall_result.2?;
    let active_id: u32 = multicall_result.3?.to();
    let static_fees = multicall_result.4?;
    let var_fees = multicall_result.5?;

    // ── Resolve tokens ──────────────────────────────────────────────────
    let (token_x, _) = token_info
        .get_or_fetch_token(provider, token_x_raw, multicall_address)
        .await?;
    let (token_y, _) = token_info
        .get_or_fetch_token(provider, token_y_raw, multicall_address)
        .await?;

    info!(
        "[Chain {}] LB Pool: TokenX={}, TokenY={}, BinStep={}, ActiveId={}",
        chain_id, token_x, token_y, bin_step, active_id
    );

    // ── Batch 2: Discover non-empty bins ─────────────────────────────────
    // Try tree bitmap discovery first, fallback to fixed window
    let bin_ids =
        match discover_bins_from_tree(provider, pool_address, block_number).await {
            Ok(ids) if !ids.is_empty() => {
                info!(
                    "[Chain {}] LB tree bitmap: discovered {} non-empty bins",
                    chain_id,
                    ids.len()
                );
                ids
            }
            Ok(_) | Err(_) => {
                let depth = bin_depth.unwrap_or(100);
                info!(
                    "[Chain {}] LB tree bitmap failed/empty, falling back to fixed window ±{}",
                    chain_id, depth
                );
                let start_id = active_id.saturating_sub(depth);
                let end_id = active_id.saturating_add(depth).min(0x00FF_FFFF);
                (start_id..=end_id).collect()
            }
        };

    // ── Batch 3: Fetch bin reserves via multicall ────────────────────────
    let mut bins = BTreeMap::new();

    for chunk in bin_ids.chunks(250) {
        let mut multicall = MulticallBuilder::new_dynamic(provider).address(multicall_address);
        for &id in chunk {
            multicall = multicall.add_dynamic(lb_instance.getBin(U24::from(id)));
        }
        let results = multicall.block(block_number).aggregate().await?;

        for (i, &id) in chunk.iter().enumerate() {
            let result = &results[i];
            let rx: u128 = result.binReserveX;
            let ry: u128 = result.binReserveY;
            if rx > 0 || ry > 0 {
                bins.insert(id, (rx, ry));
            }
        }
    }

    info!(
        "[Chain {}] LB Pool: fetched {} non-empty bins (total discovered: {})",
        chain_id,
        bins.len(),
        bin_ids.len()
    );

    let pool = LBPool::new(
        pool_address,
        token_x,
        token_y,
        bin_step,
        active_id,
        bins,
        static_fees.baseFactor,
        static_fees.filterPeriod,
        static_fees.decayPeriod,
        static_fees.reductionFactor,
        static_fees.variableFeeControl.to(),
        static_fees.protocolShare,
        static_fees.maxVolatilityAccumulator.to(),
        var_fees.volatilityAccumulator.to(),
        var_fees.volatilityReference.to(),
        var_fees.idReference.to(),
        var_fees.timeOfLastUpdate.to(),
    );

    Ok(pool)
}
