//! RPC-based fetcher for TraderJoe Liquidity Book pools.

use crate::contracts_rpc::{RpcILBPair as ILBPair, RpcILBPairV20};
use crate::lb::{detect_lb_version, LBPool, LBVersion};
use crate::TokenInfo;
use alloy::eips::BlockId;
use alloy::primitives::aliases::U24;
use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::providers::{MulticallBuilder, Provider};
use anyhow::Result;
use log::{info, warn};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Base storage slot of `_tree.level0` in LBPair v2.2.
///
/// v2.2 layout: LBToken (0-2), `_parameters`=3, `_reserves`=4,
/// `_protocolFees`=5, `_bins`=6, `_tree`=7/8/9.
/// (ReentrancyGuardUpgradeable uses ERC-7201, no sequential slot.)
const LB_TREE_BASE_SLOT_V22: u64 = 7;

/// Base storage slot of `_tree.level0` in LBPair v2.1.
///
/// v2.1 layout: LBToken (0-2), ReentrancyGuard._status=3, `_parameters`=4,
/// `_reserves`=5, `_protocolFees`=6, `_bins`=7, `_tree`=8/9/10.
/// (ReentrancyGuard has a `uint256 private _status` that shifts all slots by 1.)
const LB_TREE_BASE_SLOT_V21: u64 = 8;

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
///
/// `Ok(None)` and `Ok(Some(vec![]))` are deliberately different answers:
/// - `None` — the root bitmap is zero, so the tree is *empty*. That is the
///   ordinary storage state of a fully-drained or never-funded pair (LBPair's
///   `_burn` calls `_tree.remove(id)` as each bin empties), and it is a
///   complete, accurate answer: this pair has no non-empty bins.
/// - `Some(vec![])` — the root says some level1 group is populated, yet no
///   leaf was reachable underneath it. The tree contradicts itself, which
///   points at the storage layout rather than at the pool's liquidity.
async fn discover_bins_from_tree<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pool_address: Address,
    block_number: BlockId,
    base: u64,
) -> Result<Option<Vec<u32>>> {
    // Step 1: Read level0 (1 RPC call)
    let level0: U256 = provider
        .get_storage_at(pool_address, U256::from(base))
        .block_id(block_number)
        .await?;

    if level0.is_zero() {
        return Ok(None);
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

    Ok(Some(bin_ids))
}

/// Selector of v2.0's `TreeMath__ErrorDepthSearch()`.
///
/// v2.0's TreeMath *reverts* with this once a search runs off the end of the
/// bin tree, where v2.1+ returns a sentinel bin ID instead. It is therefore a
/// normal terminating condition of the walk, not a failure.
const TREE_MATH_ERROR_DEPTH_SEARCH: [u8; 4] = [0x10, 0xd6, 0x48, 0x61];

/// One step of the non-empty-bin walk, in the direction `swap_for_y` selects.
/// `Ok(None)` means the version reported there is no further bin that way.
///
/// v2.0 spells this `findFirstNonEmptyBinId(id, swapForY)` and takes its two
/// arguments in the opposite order to v2.1+'s `getNextNonEmptyBin(swapForY, id)`.
/// The direction semantics match (`true` walks down to lower IDs); only the
/// end-of-tree signal differs — see [`TREE_MATH_ERROR_DEPTH_SEARCH`].
async fn next_non_empty_bin<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pool_address: Address,
    version: LBVersion,
    swap_for_y: bool,
    id: u32,
    block_number: BlockId,
) -> Result<Option<u32>> {
    let next = match version {
        LBVersion::V2_0 => {
            match RpcILBPairV20::new(pool_address, provider)
                .findFirstNonEmptyBinId(U24::from(id), swap_for_y)
                .block(block_number)
                .call()
                .await
            {
                Ok(next) => next,
                // Only this one revert ends the walk; every other error —
                // transport, rate limit, any other revert — still propagates.
                Err(e)
                    if e.as_revert_data()
                        .is_some_and(|d| d[..] == TREE_MATH_ERROR_DEPTH_SEARCH) =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
        LBVersion::V2_1 | LBVersion::V2_2 => {
            ILBPair::new(pool_address, provider)
                .getNextNonEmptyBin(swap_for_y, U24::from(id))
                .block(block_number)
                .call()
                .await?
        }
    };
    Ok(Some(next.to()))
}

/// Fallback: discover all non-empty bins by walking outward from `active_id`
/// using the version's next-non-empty-bin getter in both directions.
///
/// Sequential (O(N) RPC calls), but only used when tree bitmap discovery fails
/// or the version has no corroborated tree storage layout (v2.0).
async fn discover_bins_by_walking<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pool_address: Address,
    active_id: u32,
    block_number: BlockId,
    version: LBVersion,
) -> Result<Vec<u32>> {
    let mut bin_ids: Vec<u32> = vec![active_id];

    // Walk downward (swapForY=true finds lower bin IDs)
    let mut id = active_id;
    while let Some(next) =
        next_non_empty_bin(provider, pool_address, version, true, id, block_number).await?
    {
        if next == 0 || next >= id {
            break;
        }
        bin_ids.push(next);
        id = next;
    }

    // Walk upward (swapForY=false finds higher bin IDs)
    let mut id = active_id;
    while let Some(next) =
        next_non_empty_bin(provider, pool_address, version, false, id, block_number).await?
    {
        if next == 0x00FF_FFFF || next <= id {
            break;
        }
        bin_ids.push(next);
        id = next;
    }

    bin_ids.sort();
    bin_ids.dedup();
    Ok(bin_ids)
}

/// Storage slot of `_tree.level0` for a given LB version.
///
/// v2.1 carries a `uint256 private _status` from ReentrancyGuard that shifts
/// every subsequent slot by one; v2.2 uses ERC-7201 namespaced storage and
/// has no such slot.
fn tree_base_slot(version: LBVersion) -> Option<u64> {
    match version {
        LBVersion::V2_2 => Some(LB_TREE_BASE_SLOT_V22),
        LBVersion::V2_1 => Some(LB_TREE_BASE_SLOT_V21),
        // v2.0's bin tree is a different on-chain structure whose layout is
        // not corroborated; it uses the findFirstNonEmptyBinId walk instead.
        LBVersion::V2_0 => None,
    }
}

/// Pool state normalised across LB versions.
struct LBState {
    token_x_raw: Address,
    token_y_raw: Address,
    bin_step: u16,
    active_id: u32,
    base_factor: u16,
    filter_period: u16,
    decay_period: u16,
    reduction_factor: u16,
    variable_fee_control: u32,
    protocol_share: u16,
    max_volatility_accumulator: u32,
    volatility_accumulator: u32,
    volatility_reference: u32,
    id_reference: u32,
    time_of_last_update: u64,
}

/// Read v2.0 state. v2.0 packs every fee parameter into one `feeParameters()`
/// struct and has no `getBinStep()`; bin step is field 0 of that struct.
async fn fetch_v20_state<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pool_address: Address,
    block_number: BlockId,
    multicall_address: Address,
) -> Result<LBState> {
    let lb = RpcILBPairV20::new(pool_address, provider);
    let r = provider
        .multicall()
        .address(multicall_address)
        .add(lb.tokenX()) // 0
        .add(lb.tokenY()) // 1
        .add(lb.getReservesAndId()) // 2
        .add(lb.feeParameters()) // 3
        .block(block_number)
        .try_aggregate(false)
        .await?;

    let token_x_raw = r.0?;
    let token_y_raw = r.1?;
    let reserves = r.2?;
    // `feeParameters()` returns a single *unnamed* tuple, so alloy decodes it
    // positionally rather than into a named struct. Destructuring pins the
    // arity, order and per-field width in one place: FeeParameters is
    // (binStep, baseFactor, filterPeriod, decayPeriod, reductionFactor,
    // variableFeeControl, protocolShare, maxVolatilityAccumulated,
    // volatilityAccumulated, volatilityReference, indexRef, time).
    let (
        bin_step,
        base_factor,
        filter_period,
        decay_period,
        reduction_factor,
        variable_fee_control,
        protocol_share,
        max_volatility_accumulated,
        volatility_accumulated,
        volatility_reference,
        index_ref,
        time,
    ) = r.3?;

    Ok(LBState {
        token_x_raw,
        token_y_raw,
        bin_step,
        active_id: reserves.activeId.to(),
        base_factor,
        filter_period,
        decay_period,
        reduction_factor,
        variable_fee_control: variable_fee_control.to(),
        protocol_share,
        max_volatility_accumulator: max_volatility_accumulated.to(),
        volatility_accumulator: volatility_accumulated.to(),
        volatility_reference: volatility_reference.to(),
        id_reference: index_ref.to(),
        time_of_last_update: time.to(),
    })
}

/// Read v2.1/v2.2 state. Both expose the same split getters.
async fn fetch_v21_state<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pool_address: Address,
    block_number: BlockId,
    multicall_address: Address,
) -> Result<LBState> {
    let lb = ILBPair::new(pool_address, provider);
    let r = provider
        .multicall()
        .address(multicall_address)
        .add(lb.getTokenX()) // 0
        .add(lb.getTokenY()) // 1
        .add(lb.getBinStep()) // 2
        .add(lb.getActiveId()) // 3
        .add(lb.getStaticFeeParameters()) // 4
        .add(lb.getVariableFeeParameters()) // 5
        .block(block_number)
        .try_aggregate(false)
        .await?;

    let token_x_raw = r.0?;
    let token_y_raw = r.1?;
    let bin_step = r.2?;
    let active_id: u32 = r.3?.to();
    let s = r.4?;
    let v = r.5?;

    Ok(LBState {
        token_x_raw,
        token_y_raw,
        bin_step,
        active_id,
        base_factor: s.baseFactor,
        filter_period: s.filterPeriod,
        decay_period: s.decayPeriod,
        reduction_factor: s.reductionFactor,
        variable_fee_control: s.variableFeeControl.to(),
        protocol_share: s.protocolShare,
        max_volatility_accumulator: s.maxVolatilityAccumulator.to(),
        volatility_accumulator: v.volatilityAccumulator.to(),
        volatility_reference: v.volatilityReference.to(),
        id_reference: v.idReference.to(),
        time_of_last_update: v.timeOfLastUpdate.to(),
    })
}

/// Fetch a TraderJoe Liquidity Book pool from chain.
///
/// Discovers all non-empty bins via storage bitmap reads of the on-chain TreeMath structure.
/// Falls back to sequential `getNextNonEmptyBin` walk if storage reads fail.
pub async fn fetch_lb_pool<P: Provider + Send + Sync, T: TokenInfo>(
    provider: &Arc<P>,
    pool_address: Address,
    block_number: BlockId,
    token_info: &T,
    multicall_address: Address,
    chain_id: u64,
) -> Result<LBPool> {
    info!("[Chain {}] Fetching LB pool: {}", chain_id, pool_address);

    let version = detect_lb_version(provider, pool_address, multicall_address, block_number)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{} is not a Trader Joe LB pair", pool_address))?;
    info!(
        "[Chain {}] LB pool {} detected as {:?}",
        chain_id, pool_address, version
    );

    let lb_instance = ILBPair::new(pool_address, provider);

    // ── Batch 1: Pool metadata + fee parameters ─────────────────────────
    let state = match version {
        LBVersion::V2_0 => {
            fetch_v20_state(provider, pool_address, block_number, multicall_address).await?
        }
        LBVersion::V2_1 | LBVersion::V2_2 => {
            fetch_v21_state(provider, pool_address, block_number, multicall_address).await?
        }
    };

    let hooks_parameters = if version == LBVersion::V2_2 {
        // Version detection already succeeded via this same call (it is the
        // v2.2 discriminator in `detect_lb_version`), so a failure here is a
        // genuine fetch problem, not evidence the pool lacks hooks. Swallowing
        // it would make "no hooks" and "couldn't check" indistinguishable,
        // silently disabling the warning below for a pool that may actually
        // have hooks installed.
        let h = lb_instance
            .getLBHooksParameters()
            .block(block_number)
            .call()
            .await?;
        if h.is_zero() {
            None
        } else {
            Some(h)
        }
    } else {
        None
    };
    if let Some(h) = hooks_parameters {
        warn!(
            "[Chain {}] LB pool {} has hooks installed ({}); observed behaviour \
             may deviate from pure LB math",
            chain_id, pool_address, h
        );
    }

    // ── Resolve tokens ──────────────────────────────────────────────────
    let (token_x, _) = token_info
        .get_or_fetch_token(provider, state.token_x_raw, multicall_address)
        .await?;
    let (token_y, _) = token_info
        .get_or_fetch_token(provider, state.token_y_raw, multicall_address)
        .await?;

    info!(
        "[Chain {}] LB Pool: TokenX={}, TokenY={}, BinStep={}, ActiveId={}",
        chain_id, token_x, token_y, state.bin_step, state.active_id
    );

    // ── Batch 2: Discover non-empty bins ─────────────────────────────────
    let bin_ids: Vec<u32> = match tree_base_slot(version) {
        Some(base) => {
            match discover_bins_from_tree(provider, pool_address, block_number, base).await {
                Ok(Some(ids)) => {
                    if ids.is_empty() {
                        // A non-zero root with no reachable leaves is the one
                        // shape a real pool cannot take: a drained pair zeroes
                        // its root (handled by the `None` arm below), so the
                        // root and the leaves genuinely disagree here.
                        return Err(anyhow::anyhow!(
                            "LB bin tree at slot {} for {} ({:?}): root bitmap is non-zero \
                             but no leaf bin was reachable under it — storage layout may \
                             have changed",
                            base,
                            pool_address,
                            version
                        ));
                    }
                    info!(
                        "[Chain {}] LB tree bitmap ({:?}, slot {}): discovered {} non-empty bins",
                        chain_id,
                        version,
                        base,
                        ids.len()
                    );
                    ids
                }
                Ok(None) => {
                    // Empty tree: a fully-drained or never-funded pair. Register
                    // it with no bins — `calculate_output` then returns
                    // "Insufficient liquidity in LB pool", which is the correct
                    // quote. Erroring instead would fail every retry
                    // deterministically and abandon every remaining pool in the
                    // caller's fetch.
                    info!(
                        "[Chain {}] LB bin tree ({:?}, slot {}) for {} is empty; \
                         registering pool with no bins",
                        chain_id, version, base, pool_address
                    );
                    Vec::new()
                }
                Err(e) => {
                    // The storage reads themselves failed. v2.1 and v2.2 both
                    // expose `getNextNonEmptyBin`, so answer with the walk
                    // rather than giving up on the pool.
                    warn!(
                        "[Chain {}] LB bin tree read failed for {} ({:?}, slot {}): {} — \
                         falling back to the bin walk",
                        chain_id, pool_address, version, base, e
                    );
                    let ids = discover_bins_by_walking(
                        provider,
                        pool_address,
                        state.active_id,
                        block_number,
                        version,
                    )
                    .await
                    .map_err(|walk_err| {
                        anyhow::anyhow!(
                            "LB bin discovery failed for {} ({:?}): tree read at slot {} \
                             failed ({}), and the walk fallback also failed ({})",
                            pool_address,
                            version,
                            base,
                            e,
                            walk_err
                        )
                    })?;
                    info!(
                        "[Chain {}] LB bin walk fallback ({:?}): discovered {} non-empty bins",
                        chain_id,
                        version,
                        ids.len()
                    );
                    ids
                }
            }
        }
        None => {
            // The walk seeds itself with `active_id`, so it never returns an
            // empty vector; a pool whose active bin is empty simply drops out
            // of `bins` in Batch 3 below.
            let ids = discover_bins_by_walking(
                provider,
                pool_address,
                state.active_id,
                block_number,
                version,
            )
            .await?;
            info!(
                "[Chain {}] LB bin walk ({:?}): discovered {} non-empty bins",
                chain_id,
                version,
                ids.len()
            );
            ids
        }
    };

    // ── Batch 3: Fetch bin reserves via multicall ────────────────────────
    let mut bins = BTreeMap::new();
    let lb20_instance = RpcILBPairV20::new(pool_address, provider);

    for chunk in bin_ids.chunks(250) {
        match version {
            LBVersion::V2_0 => {
                let mut multicall =
                    MulticallBuilder::new_dynamic(provider).address(multicall_address);
                for &id in chunk {
                    multicall = multicall.add_dynamic(lb20_instance.getBin(U24::from(id)));
                }
                let results = multicall.block(block_number).aggregate().await?;
                for (i, &id) in chunk.iter().enumerate() {
                    // v2.0 returns uint256; real bin reserves always fit u128.
                    let rx: u128 = results[i].reserveX.try_into().unwrap_or(0);
                    let ry: u128 = results[i].reserveY.try_into().unwrap_or(0);
                    if rx > 0 || ry > 0 {
                        bins.insert(id, (rx, ry));
                    }
                }
            }
            LBVersion::V2_1 | LBVersion::V2_2 => {
                let mut multicall =
                    MulticallBuilder::new_dynamic(provider).address(multicall_address);
                for &id in chunk {
                    multicall = multicall.add_dynamic(lb_instance.getBin(U24::from(id)));
                }
                let results = multicall.block(block_number).aggregate().await?;
                for (i, &id) in chunk.iter().enumerate() {
                    let rx: u128 = results[i].binReserveX;
                    let ry: u128 = results[i].binReserveY;
                    if rx > 0 || ry > 0 {
                        bins.insert(id, (rx, ry));
                    }
                }
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
        state.bin_step,
        state.active_id,
        bins,
        state.base_factor,
        state.filter_period,
        state.decay_period,
        state.reduction_factor,
        state.variable_fee_control,
        state.protocol_share,
        state.max_volatility_accumulator,
        state.volatility_accumulator,
        state.volatility_reference,
        state.id_reference,
        state.time_of_last_update,
    );

    Ok(pool
        .with_version(version)
        .with_hooks_parameters(hooks_parameters))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The selector is hand-written from a signature the compiler never sees,
    /// so pin it to the keccak of that signature: a typo here would turn
    /// "the walk finished" into "propagate the error" (or the reverse).
    #[test]
    fn tree_math_depth_search_selector_matches_signature() {
        assert_eq!(
            keccak256("TreeMath__ErrorDepthSearch()")[..4],
            TREE_MATH_ERROR_DEPTH_SEARCH
        );
    }
}
