use crate::contracts_rpc::RpcIUniswapV2Pair as IUniswapV2Pair;
use crate::contracts_rpc::RpcIV2PairUint256 as IV2PairUint256;
use crate::contracts_rpc::RpcIVeloPoolFactory as IVeloPoolFactory;
use crate::contracts_rpc::RpcUniswapV2FactoryGetFeeOnlyPair as UniswapV2FactoryGetFeeOnlyPair;
use crate::contracts_rpc::RpcUniswapV2FactoryGetFeePool as UniswapV2FactoryGetFeePool;
use crate::contracts_rpc::RpcUniswapV2FactoryPairFee as UniswapV2FactoryPairFee;
use crate::contracts_rpc::RpcVolatileStableFeeInFactory as VolatileStableFeeInFactory;
use crate::contracts_rpc::RpcVolatileStableGetFee as VolatileStableGetFee;
use crate::v2::{
    get_aero_factories_by_chain_id, get_v2_factory_fee_by_chain_id, UniswapV2Pool, V2PoolType,
};
use crate::TokenInfo;
use alloy::{
    eips::BlockId,
    primitives::{Address, U160, U256},
    providers::{MulticallBuilder, Provider},
};
use anyhow::{anyhow, Result};
use log::info;
use std::{collections::HashMap, sync::Arc};

const FACTORY_STORAGE_SLOT: u64 = 0xb;
const GET_FEE_MULTIPLIER: u128 = 100;
const GET_FEE_MAX: u128 = 10000;
const REVERSE_FEE_MAX: u128 = 5000;
/// The basis `UniswapV2Pool` stores its fee in: 3000 is 0.3%.
const FEE_BASIS: u128 = 1_000_000;
/// A fee this crate will not believe -- 10% of notional. Above it the reading
/// is a misparse, and pricing on it is worse than refusing the pool.
const MAX_SANE_FEE: u128 = 100_000;

/// Interpret a raw `fee()` reading, without wrapping.
///
/// Three conventions share this one number space: a fee in 1e4 basis
/// (30 = 0.3%), its complement in 1e4 basis (9970 = 0.3%), and a fee already
/// in the 1e6 basis this crate stores (3000 = 0.3%).
///
/// A value above `GET_FEE_MAX` can only be the third: as a 1e4 fee it would
/// exceed 100%, and as a complement it would be negative. That case used to
/// take the complement branch, where the bare `GET_FEE_MAX - raw` wrapped to
/// near `U256::MAX`; every later `fee.to::<u128>()` then panicked, 29k times
/// in one 42-hour run, aborting the whole simulation task each time.
///
/// The first two remain genuinely ambiguous below `GET_FEE_MAX` -- 9970 reads
/// as 0.3% one way and 0.997% the other -- which is why
/// [`calibrate_v2_fee`] is preferred wherever the pool will quote itself.
fn normalise_reported_fee(raw: U256) -> U256 {
    let max = U256::from(GET_FEE_MAX);
    if raw > max {
        raw
    } else if raw > U256::from(REVERSE_FEE_MAX) {
        // `raw <= max` here, so this cannot underflow.
        (max - raw) * U256::from(GET_FEE_MULTIPLIER)
    } else {
        raw * U256::from(GET_FEE_MULTIPLIER)
    }
}

/// Back-solve a volatile pool's fee from its own `getAmountOut`.
///
/// Preferred over [`normalise_reported_fee`] because it assumes no convention
/// at all. Three pools on one Avalanche factory were misread at once: two
/// reported 15000 and wrapped, and a third reported 5000 and was priced at
/// 50% when it charges 0.5%.
///
/// For the constant-product-with-fee curve
///
///     out = (ain * (1-f) * r1) / (r0 + ain * (1-f))
///
/// let x = ain*(1-f). Then out*(r0 + x) = x*r1, so x = out*r0 / (r1 - out) and
/// f = 1 - x/ain. That inverts the whole curve, slippage included -- it is not
/// a small-trade approximation, and it is exact even when the probe exceeds
/// the reserves.
///
/// The probe must still be a large fraction of the pool, because
/// `getAmountOut` returns an integer and on a small probe the truncation
/// swamps the answer: a 1e6 probe read 15169 where the fee was 15000, and 1e18
/// read 15022 against a 6-decimal counter-token. Two proportional probes go
/// out in one multicall and must agree, so a pool on some other curve (or one
/// that lies) falls back instead of being priced on a guess.
async fn calibrate_v2_fee<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pool_address: Address,
    token0: Address,
    reserve0: U256,
    reserve1: U256,
    multicall_address: Address,
    block_number: BlockId,
) -> Option<U256> {
    if reserve0.is_zero() || reserve1.is_zero() {
        return None;
    }
    let probes = [reserve0 / U256::from(2), reserve0 * U256::from(4)];
    let pair = IV2PairUint256::new(pool_address, provider);
    let quotes = provider
        .multicall()
        .address(multicall_address)
        .add(pair.getAmountOut(probes[0], token0))
        .add(pair.getAmountOut(probes[1], token0))
        .block(block_number)
        .try_aggregate(false)
        .await
        .ok()?;

    let solve = |ain: U256, out: U256| -> Option<U256> {
        if ain.is_zero() || out.is_zero() || out >= reserve1 {
            return None;
        }
        let scale = U256::from(FEE_BASIS);
        let denominator = ain * (reserve1 - out);
        // Rounded, not truncated: (1-f) scaled into the stored basis.
        let one_minus_f = (out * reserve0 * scale + denominator / U256::from(2)) / denominator;
        scale.checked_sub(one_minus_f)
    };

    let low = solve(probes[0], quotes.0.ok()?)?;
    let high = solve(probes[1], quotes.1.ok()?)?;
    if low != high || low.is_zero() || low > U256::from(MAX_SANE_FEE) {
        return None;
    }
    Some(low)
}

/// Fetches pool data for a V2 pool
pub async fn fetch_v2_pool<P: Provider + Send + Sync, T: TokenInfo>(
    provider: &Arc<P>,
    pool_address: Address,
    block_number: BlockId,
    token_info: &T,
    multicall_address: Address,
    chain_id: u64,
    factory_to_fee: &HashMap<String, u64>,
    aero_factories: &[Address],
) -> Result<UniswapV2Pool> {
    info!("[Chain {}] Fetching V2 pool: {}", chain_id, pool_address);
    let pair_instance = IUniswapV2Pair::new(pool_address, &provider);
    let uint256_pair_instance = IV2PairUint256::new(pool_address, &provider);
    let volatile_stable_fee_in_factory_instance =
        VolatileStableFeeInFactory::new(pool_address, &provider);
    let volatile_stable_get_fee_instance = VolatileStableGetFee::new(pool_address, &provider);
    let multicall_result = provider
        .multicall()
        .address(multicall_address)
        .add(pair_instance.token0()) // 0
        .add(pair_instance.token1()) // 1
        .add(pair_instance.getReserves()) // 2
        .add(pair_instance.factory()) // 3
        .add(pair_instance.fee()) // 4
        .add(uint256_pair_instance.getReserves()) // 5
        .add(volatile_stable_fee_in_factory_instance.stable()) // 6
        .add(volatile_stable_get_fee_instance.getFee()) // 7
        .add(volatile_stable_get_fee_instance.isStable()) // 8
        .add(pair_instance.swapFee()) // 9
        .block(block_number)
        .try_aggregate(false)
        .await?;

    // Tokens
    let token0_address = multicall_result.0.map_err(|e| anyhow!("token0() failed for {}: {}", pool_address, e))?;
    let token1_address = multicall_result.1.map_err(|e| anyhow!("token1() failed for {}: {}", pool_address, e))?;
    // Factory
    let mut factory = multicall_result.3.unwrap_or(Address::ZERO);
    // Reserves
    let (reserve0, reserve1) = if let Ok(reserves_result) = multicall_result.2 {
        (
            U256::from(reserves_result._reserve0),
            U256::from(reserves_result._reserve1),
        )
    } else if let Ok(reserves_result) = multicall_result.5 {
        (reserves_result._reserve0, reserves_result._reserve1)
    } else {
        return Err(anyhow!("Failed to get reserves"));
    };

    // Is Stable
    let is_stable = if let Ok(is_stable_result) = multicall_result.6 {
        is_stable_result
    } else if let Ok(is_stable_result) = multicall_result.8 {
        is_stable_result
    } else {
        false
    };

    // Fee
    //
    // Where the pool will quote itself, that beats any reading of `fee()` --
    // see `calibrate_v2_fee`. Only the on-chain-read branches are calibrated:
    // an operator's `factory_to_fee` override is an explicit choice and is
    // left alone, and the stable curve is x^3y+y^3x, which the
    // constant-product inversion does not describe.
    let reads_fee_on_chain =
        multicall_result.4.is_ok() || multicall_result.7.is_ok() || multicall_result.9.is_ok();
    let calibrated = if reads_fee_on_chain && !is_stable {
        calibrate_v2_fee(
            provider,
            pool_address,
            token0_address,
            reserve0,
            reserve1,
            multicall_address,
            block_number,
        )
        .await
    } else {
        None
    };

    let fee = if let Ok(fee_result) = multicall_result.4 {
        calibrated.unwrap_or_else(|| normalise_reported_fee(fee_result))
    } else if let Ok(fee_result) = multicall_result.7 {
        calibrated.unwrap_or_else(|| fee_result * U256::from(GET_FEE_MULTIPLIER))
    } else if let Ok(fee_result) = multicall_result.9 {
        calibrated.unwrap_or_else(|| U256::from(fee_result) * U256::from(GET_FEE_MULTIPLIER))
    } else {
        factory = if !factory.is_zero() {
            factory
        } else {
            let factory_storage = provider
                .get_storage_at(pool_address, U256::from(FACTORY_STORAGE_SLOT))
                .await?;
            let factory = Address::from(U160::from(factory_storage));
            info!(
                "[Chain {}] Pool factory from storage: {}",
                chain_id, factory
            );
            factory
        };
        // Try to get fee from factory to fee map (config override)
        // Case-insensitive lookup: callers may store keys as checksummed or lowercase
        let factory_str = factory.to_string().to_lowercase();
        match factory_to_fee
            .iter()
            .find(|(k, _)| k.to_lowercase() == factory_str)
        {
            Some((_, fee)) => U256::from(*fee),
            None => match get_v2_factory_fee_by_chain_id(chain_id, &factory) {
                // Hardcoded chain-specific factory fee
                Ok(fee) => fee,
                Err(_) => {
                    // Dynamic factory RPC query
                    let fee = if let Some(fee) = get_v2_fee_from_factory(
                        provider,
                        factory,
                        pool_address,
                        is_stable,
                        multicall_address,
                        block_number,
                    )
                    .await
                    {
                        fee
                    } else {
                        // Merge config aero factories with hardcoded fallback
                        let hardcoded_aero = get_aero_factories_by_chain_id(chain_id);
                        let all_aero: Vec<Address> = aero_factories
                            .iter()
                            .copied()
                            .chain(
                                hardcoded_aero
                                    .into_iter()
                                    .filter(|a| !aero_factories.contains(a)),
                            )
                            .collect();

                        let mut multicall =
                            MulticallBuilder::new_dynamic(provider).address(multicall_address);
                        for aero_factory in &all_aero {
                            let factory_instance = IVeloPoolFactory::new(*aero_factory, &provider);
                            multicall = multicall.add_dynamic(factory_instance.getPair(
                                token0_address,
                                token1_address,
                                is_stable,
                            ));
                        }

                        let results = multicall.block(block_number).try_aggregate(false).await?;
                        let mut fee_found = None;
                        for (i, result) in results.into_iter().enumerate() {
                            if let Ok(pool_address_result) = result {
                                if pool_address_result.eq(&pool_address) {
                                    if let Some(fee) = get_v2_fee_from_factory(
                                        provider,
                                        all_aero[i],
                                        pool_address,
                                        is_stable,
                                        multicall_address,
                                        block_number,
                                    )
                                    .await
                                    {
                                        fee_found = Some(fee);
                                        factory = all_aero[i];
                                        info!(
                                            "[Chain {}] Found Aero factory: {}",
                                            chain_id, factory
                                        );
                                        break;
                                    }
                                };
                            }
                        }
                        if let Some(fee) = fee_found {
                            fee
                        } else {
                            return Err(anyhow!(
                                "Could not determine fee for pool {} with factory {} on chain {}",
                                pool_address,
                                factory,
                                chain_id
                            ));
                        }
                    };

                    fee
                }
            },
        }
    };

    // Refuse a fee no V2 pool charges rather than quoting through it. The
    // readings this guards against are misparses, and a silently mispriced
    // pool costs more than an absent one: 0x903c3ed1 spent a 42-hour run
    // priced at 50% when it charges 0.5%, quietly producing nothing.
    if fee > U256::from(MAX_SANE_FEE) {
        return Err(anyhow!(
            "pool {} resolved to a fee of {} ({} basis): implausible, so the \
             reading is a misparse rather than a pool worth pricing",
            pool_address,
            fee,
            FEE_BASIS
        ));
    }

    // Pool type
    let pool_type = if is_stable {
        info!("[Chain {}] Pool is stable", chain_id);
        V2PoolType::Stable
    } else {
        V2PoolType::UniswapV2
    };

    // Create token objects (you'll need to fetch token details)
    let (token0, decimals0) = token_info
        .get_or_fetch_token(provider, token0_address, multicall_address)
        .await?;
    let (token1, decimals1) = token_info
        .get_or_fetch_token(provider, token1_address, multicall_address)
        .await?;
    // Create and return V2 pool
    info!(
        "[Chain {}] {} Token0: {}, Token1: {}, Fee: {}, Factory: {}",
        chain_id,
        if is_stable { "Stable Pool" } else { "V2 Pool" },
        token0,
        token1,
        fee,
        factory,
    );
    Ok(UniswapV2Pool::new(
        pool_type,
        pool_address,
        token0,
        token1,
        decimals0,
        decimals1,
        reserve0,
        reserve1,
        fee,
    ))
}

async fn get_v2_fee_from_factory<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    factory: Address,
    pool_address: Address,
    is_stable: bool,
    multicall_address: Address,
    block_number: BlockId,
) -> Option<U256> {
    // If factory is not in factory to fee map, try to get fee from factory
    let factory_get_fee_pool_instance = UniswapV2FactoryGetFeePool::new(factory, &provider);
    let factory_pair_fee_instance = UniswapV2FactoryPairFee::new(factory, &provider);
    let factory_get_fee_only_pair_instance =
        UniswapV2FactoryGetFeeOnlyPair::new(factory, &provider);

    let multicall_result = provider
        .multicall()
        .address(multicall_address)
        .add(factory_get_fee_pool_instance.getFee(pool_address, is_stable)) // 0
        .add(factory_pair_fee_instance.getFee(is_stable)) // 1
        .add(factory_get_fee_only_pair_instance.getFee(pool_address)) // 2
        .block(block_number)
        .try_aggregate(false)
        .await
        .ok()?;

    let fee = if let Ok(fee_result) = multicall_result.0 {
        Some(U256::from(fee_result * U256::from(GET_FEE_MULTIPLIER)))
    } else if let Ok(fee_result) = multicall_result.1 {
        Some(U256::from(fee_result * U256::from(GET_FEE_MULTIPLIER)))
    } else if let Ok(fee_result) = multicall_result.2 {
        Some(U256::from(fee_result * U256::from(GET_FEE_MULTIPLIER)))
    } else {
        None
    };

    fee
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reading that wrapped. 15000 cannot be a 1e4-basis fee (that is
    /// 150%) nor a complement (that is negative), so it is already in the
    /// stored basis and must pass straight through rather than reach a
    /// subtraction.
    #[test]
    fn a_reading_above_the_1e4_ceiling_passes_through() {
        assert_eq!(normalise_reported_fee(U256::from(15000)), U256::from(15000));
        assert_eq!(normalise_reported_fee(U256::from(30000)), U256::from(30000));
    }

    /// The regression that mattered: 29k panics in one run came from a fee
    /// stored near U256::MAX. Whatever else changes here, no reading may
    /// produce a fee beyond 100% of notional.
    #[test]
    fn no_reading_wraps() {
        for raw in [
            0u64, 1, 30, 5000, 5001, 9970, 9999, 10000, 10001, 15000, 1_000_000,
        ] {
            let fee = normalise_reported_fee(U256::from(raw));
            assert!(
                fee <= U256::from(FEE_BASIS),
                "raw {raw} produced {fee}, beyond 100% of notional"
            );
        }
    }

    /// The two conventions that already worked must keep working.
    #[test]
    fn the_established_readings_are_unchanged() {
        // A fee in 1e4 basis: 30 is 0.3%.
        assert_eq!(normalise_reported_fee(U256::from(30)), U256::from(3000));
        // Its complement in 1e4 basis: 9970 is also 0.3%.
        assert_eq!(normalise_reported_fee(U256::from(9970)), U256::from(3000));
    }
}
