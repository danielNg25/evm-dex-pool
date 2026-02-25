use crate::contracts_rpc::RpcIUniswapV2Pair as IUniswapV2Pair;
use crate::contracts_rpc::RpcIV2PairUint256 as IV2PairUint256;
use crate::contracts_rpc::RpcIVeloPoolFactory as IVeloPoolFactory;
use crate::contracts_rpc::RpcUniswapV2FactoryGetFeeOnlyPair as UniswapV2FactoryGetFeeOnlyPair;
use crate::contracts_rpc::RpcUniswapV2FactoryGetFeePool as UniswapV2FactoryGetFeePool;
use crate::contracts_rpc::RpcUniswapV2FactoryPairFee as UniswapV2FactoryPairFee;
use crate::contracts_rpc::RpcVolatileStableFeeInFactory as VolatileStableFeeInFactory;
use crate::contracts_rpc::RpcVolatileStableGetFee as VolatileStableGetFee;
use crate::v2::{get_aero_factories_by_chain_id, get_v2_factory_fee_by_chain_id, UniswapV2Pool, V2PoolType};
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
    let (token0_address, token1_address) =
        (multicall_result.0.unwrap(), multicall_result.1.unwrap());
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
    let fee = if let Ok(mut fee_result) = multicall_result.4 {
        if fee_result.gt(&U256::from(REVERSE_FEE_MAX)) {
            fee_result = U256::from(GET_FEE_MAX) - fee_result;
        }
        U256::from(fee_result * U256::from(GET_FEE_MULTIPLIER))
    } else if let Ok(fee_result) = multicall_result.7 {
        U256::from(fee_result * U256::from(GET_FEE_MULTIPLIER))
    } else if let Ok(fee_result) = multicall_result.9 {
        U256::from(U256::from(fee_result) * U256::from(GET_FEE_MULTIPLIER))
    } else {
        factory = if !factory.is_zero() {
            factory
        } else {
            let factory_storage = provider
                .get_storage_at(pool_address, U256::from(FACTORY_STORAGE_SLOT))
                .await?;
            let factory = Address::from(U160::from(factory_storage));
            info!("[Chain {}] Pool factory from storage: {}", chain_id, factory);
            factory
        };
        // Try to get fee from factory to fee map (config override)
        // Case-insensitive lookup: callers may store keys as checksummed or lowercase
        let factory_str = factory.to_string().to_lowercase();
        match factory_to_fee.iter().find(|(k, _)| k.to_lowercase() == factory_str) {
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
                            let factory_instance =
                                IVeloPoolFactory::new(*aero_factory, &provider);
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
                                        info!("[Chain {}] Found Aero factory: {}", chain_id, factory);
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

    // Pool type
    let pool_type = if is_stable {
        info!("[Chain {}] Pool is stable", chain_id);
        V2PoolType::Stable
    } else {
        V2PoolType::UniswapV2
    };

    // Create token objects (you'll need to fetch token details)
    let (token0, decimals0) =
        token_info.get_or_fetch_token(provider, token0_address, multicall_address).await?;
    let (token1, decimals1) =
        token_info.get_or_fetch_token(provider, token1_address, multicall_address).await?;
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
