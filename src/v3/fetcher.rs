use crate::contracts_rpc::RpcAlgebraPoolFeeInState as AlgebraPoolFeeInState;
use crate::contracts_rpc::RpcAlgebraTwoSideFee as AlgebraTwoSideFee;
use crate::contracts_rpc::RpcAlgebraV3Pool as AlgebraV3Pool;
use crate::contracts_rpc::RpcCLPPool as CLPPool;
use crate::contracts_rpc::RpcIQuoter as IQuoter;
use crate::contracts_rpc::RpcIUniswapV3Pool as IUniswapV3Pool;
use crate::v3::{
    get_ramses_quoter, is_ramses_factory, Tick, UniswapV3Pool, V3PoolType, MAX_TICK_I32,
    MIN_TICK_I32, RAMSES_FACTOR,
};
use crate::{PoolInterface, TokenInfo};
use alloy::primitives::{aliases::U24, Address, Signed, U160, U256};
use alloy::primitives::{Uint, U128};
use alloy::{
    eips::BlockId,
    providers::{MulticallBuilder, Provider},
};
use anyhow::Result;
use log::info;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Fetches pool data for a V3 pool
pub async fn fetch_v3_pool<P: Provider + Send + Sync, T: TokenInfo>(
    provider: &Arc<P>,
    pool_address: Address,
    block_number: BlockId,
    token_info: &T,
    multicall_address: Address,
) -> Result<UniswapV3Pool> {
    let mut v3_pool_type = V3PoolType::UniswapV3;
    let uniswapv3_pool_instance = IUniswapV3Pool::new(pool_address, &provider);
    let clp_pool_instance = CLPPool::new(pool_address, &provider);
    let algebra_v3_pool_instance = AlgebraV3Pool::new(pool_address, &provider);
    let algebra_two_side_fee_pool_instance = AlgebraTwoSideFee::new(pool_address, &provider);
    let algebra_pool_fee_in_state_instance = AlgebraPoolFeeInState::new(pool_address, &provider);

    let multicall_result = provider
        .multicall()
        .address(multicall_address)
        .add(uniswapv3_pool_instance.token0()) // 0
        .add(uniswapv3_pool_instance.token1()) // 1
        .add(uniswapv3_pool_instance.fee()) // 2
        .add(uniswapv3_pool_instance.tickSpacing()) // 3
        .add(uniswapv3_pool_instance.slot0()) // 4
        .add(uniswapv3_pool_instance.liquidity()) // 5
        .add(uniswapv3_pool_instance.factory()) // 6
        .add(clp_pool_instance.slot0()) // this does not has feeProtocol param // 7
        .add(algebra_v3_pool_instance.fee()) // 8
        .add(algebra_v3_pool_instance.globalState()) // 9
        .add(algebra_two_side_fee_pool_instance.globalState()) // 10
        .add(algebra_two_side_fee_pool_instance.activeIncentive()) // 11
        .add(algebra_pool_fee_in_state_instance.globalState()) // 12
        .block(block_number)
        .try_aggregate(false)
        .await?;
    let (token0, token1, fee, tick_spacing, sqrt_price_x96, tick, liquidity, factory) =
        if let Ok(slot0_result) = multicall_result.7 {
            (
                multicall_result.0.unwrap(),
                multicall_result.1.unwrap(),
                multicall_result.2.unwrap(),
                multicall_result.3.unwrap(),
                slot0_result.sqrtPriceX96,
                slot0_result.tick,
                multicall_result.5.unwrap(),
                multicall_result.6.unwrap(),
            )
        } else if let Ok(slot0_result) = multicall_result.12 {
            let fee: U24 = U24::from(slot0_result.fee);
            v3_pool_type = V3PoolType::AlgebraPoolFeeInState;
            (
                multicall_result.0.unwrap(),
                multicall_result.1.unwrap(),
                fee,
                multicall_result.3.unwrap(),
                slot0_result.price,
                slot0_result.tick,
                multicall_result.5.unwrap(),
                multicall_result.6.unwrap(),
            )
        } else if let Ok(_) = multicall_result.11 {
            let slot0_result = multicall_result.10.unwrap();
            let fee: U24 = if slot0_result.feeZto > slot0_result.feeOtz {
                U24::from(slot0_result.feeZto)
            } else {
                U24::from(slot0_result.feeOtz)
            };
            v3_pool_type = V3PoolType::AlgebraTwoSideFee;
            (
                multicall_result.0.unwrap(),
                multicall_result.1.unwrap(),
                fee,
                multicall_result.3.unwrap(),
                slot0_result.price,
                slot0_result.tick,
                multicall_result.5.unwrap(),
                multicall_result.6.unwrap(),
            )
        } else if let Ok(slot0_result) = multicall_result.9 {
            v3_pool_type = V3PoolType::AlgebraV3;
            (
                multicall_result.0.unwrap(),
                multicall_result.1.unwrap(),
                U24::from(multicall_result.8.unwrap()),
                multicall_result.3.unwrap(),
                slot0_result.price,
                slot0_result.tick,
                multicall_result.5.unwrap(),
                multicall_result.6.unwrap(),
            )
        } else {
            let slot0_result = multicall_result.4.unwrap();
            let factory = multicall_result.6.unwrap();
            if is_ramses_factory(factory) {
                v3_pool_type = V3PoolType::RamsesV2;
            }
            (
                multicall_result.0.unwrap(),
                multicall_result.1.unwrap(),
                multicall_result.2.unwrap(),
                multicall_result.3.unwrap(),
                slot0_result.sqrtPriceX96,
                slot0_result.tick,
                multicall_result.5.unwrap(),
                factory,
            )
        };

    // Create token objects (you'll need to fetch token details)
    let (token0, _) =
        token_info.get_or_fetch_token(provider, token0, multicall_address).await?;
    let (token1, _) =
        token_info.get_or_fetch_token(provider, token1, multicall_address).await?;

    info!(
        "V3 Pool {:?}: Token0: {}, Token1: {}, Fee: {}, Factory: {}, Tick: {}, Liquidity: {}",
        v3_pool_type, token0, token1, fee, factory, tick, liquidity
    );
    // Create and return V3 pool
    let mut pool = UniswapV3Pool::new(
        pool_address,
        token0,
        token1,
        fee,
        tick_spacing.as_i32(),
        sqrt_price_x96.to::<U160>(),
        tick.as_i32(),
        liquidity,
        factory,
        v3_pool_type,
    );

    fetch_v3_ticks(provider, &mut pool, block_number, multicall_address).await?;

    if pool.pool_type == V3PoolType::RamsesV2 {
        let ratio_conversion_factor =
            calculate_ratio_conversion_factor(&pool, provider, block_number).await?;
        info!(
            "Ratio conversion factor: {}",
            ratio_conversion_factor.to::<U128>()
        );
        pool.update_ratio_conversion_factor(ratio_conversion_factor);
    }

    Ok(pool)
}

/// Fetches tick data for a V3 pool
pub async fn fetch_v3_ticks<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pool: &mut UniswapV3Pool,
    block_number: BlockId,
    multicall_address: Address,
) -> Result<()> {
    let mut tick_indices = Vec::new();

    match pool.pool_type {
        V3PoolType::UniswapV3 | V3PoolType::RamsesV2 | V3PoolType::PancakeV3 => {
            // Fetch word bitmap
            let min_word = pool.tick_to_word(MIN_TICK_I32);
            let max_word = pool.tick_to_word(MAX_TICK_I32);

            // Fetching bitMaps from their position
            let mut word_pos_indices: Vec<i32> = vec![];

            // Split word bitmap fetching into chunks
            let mut all_bitmaps = Vec::new();
            let contract = IUniswapV3Pool::new(pool.address, provider);
            for chunk in (min_word..=max_word).collect::<Vec<_>>().chunks(250) {
                let mut multicall =
                    MulticallBuilder::new_dynamic(provider).address(multicall_address);
                for &word_pos in chunk {
                    word_pos_indices.push(word_pos);
                    multicall = multicall.add_dynamic(contract.tickBitmap(word_pos as i16));
                }
                let results = multicall.block(block_number).aggregate().await?;
                all_bitmaps.extend(results.into_iter().map(|tick_bitmap| tick_bitmap));
            }

            for (j, word_pos) in word_pos_indices.iter().enumerate() {
                let bitmap = all_bitmaps[j];

                if bitmap != U256::ZERO {
                    for i in 0..256 {
                        let bit = U256::from(1u64);
                        let initialized = (bitmap & (bit << i)) != U256::ZERO;

                        if initialized {
                            let tick_index = (word_pos * 256 + i as i32) * pool.tick_spacing;
                            tick_indices.push(tick_index);
                        }
                    }
                }
            }
        }
        V3PoolType::AlgebraV3 => {
            // Algebra V3 approach: navigate through 3-level tree structure
            let contract = AlgebraV3Pool::new(pool.address, provider);
            // Step 1: Fetch the root of the tick tree
            let tick_tree_root: u32 = contract.tickTreeRoot().block(block_number).call().await?;
            if tick_tree_root == 0 {
                // No initialized ticks
                pool.ticks = BTreeMap::new();
                return Ok(());
            }

            // Step 2: Find active second layer indices from root
            let mut second_layer_indices = Vec::new();
            for root_bit in 0..32 {
                if (tick_tree_root & (1 << root_bit)) != 0 {
                    second_layer_indices.push(root_bit as i16);
                }
            }

            // Step 3: Fetch second layer bitmaps
            let mut second_layer_multicall =
                MulticallBuilder::new_dynamic(provider).address(multicall_address);
            for &second_layer_index in &second_layer_indices {
                second_layer_multicall = second_layer_multicall
                    .add_dynamic(contract.tickTreeSecondLayer(second_layer_index));
            }
            let second_layer_results = second_layer_multicall
                .block(block_number)
                .aggregate()
                .await?;

            // Step 4: Find active tick table indices from second layer
            let mut tick_table_indices = Vec::new();
            const SECOND_LAYER_OFFSET: i16 = 3466; // ceil(-MIN_TICK / 256)

            for (i, &second_layer_index) in second_layer_indices.iter().enumerate() {
                let second_layer_bitmap: U256 = second_layer_results[i];

                if second_layer_bitmap != U256::ZERO {
                    for second_bit in 0..256 {
                        if (second_layer_bitmap & (U256::from(1u64) << second_bit)) != U256::ZERO {
                            let leaf_index = second_layer_index as i32 * 256 + second_bit as i32;
                            let tick_table_index = leaf_index - SECOND_LAYER_OFFSET as i32;
                            tick_table_indices.push(tick_table_index as i16);
                        }
                    }
                }
            }

            // Step 5: Fetch tick table bitmaps (leaf layer)
            let mut tick_table_multicall =
                MulticallBuilder::new_dynamic(provider).address(multicall_address);
            for &tick_table_index in &tick_table_indices {
                tick_table_multicall =
                    tick_table_multicall.add_dynamic(contract.tickTable(tick_table_index));
            }
            let tick_table_results = tick_table_multicall.block(block_number).aggregate().await?;

            // Step 6: Find all initialized tick indices
            for (i, &tick_table_index) in tick_table_indices.iter().enumerate() {
                let tick_table_bitmap: U256 = tick_table_results[i];

                if tick_table_bitmap != U256::ZERO {
                    for tick_bit in 0..256 {
                        if (tick_table_bitmap & (U256::from(1u64) << tick_bit)) != U256::ZERO {
                            let tick_index = (tick_table_index as i32)
                                .wrapping_mul(256)
                                .wrapping_add(tick_bit as i32);

                            if tick_index >= MIN_TICK_I32 && tick_index <= MAX_TICK_I32 {
                                tick_indices.push(tick_index);
                            }
                        }
                    }
                }
            }
        }
        V3PoolType::AlgebraTwoSideFee | V3PoolType::AlgebraPoolFeeInState => {
            // Algebra Two Side Fee approach: navigate through 3-level tree structure
            // Fetch word bitmap
            let min_word = pool.tick_to_word(MIN_TICK_I32);
            let max_word = pool.tick_to_word(MAX_TICK_I32);

            // Fetching bitMaps from their position
            let mut word_pos_indices: Vec<i32> = vec![];

            // Split word bitmap fetching into chunks
            let mut all_bitmaps = Vec::new();
            let contract = AlgebraTwoSideFee::new(pool.address, provider);
            for chunk in (min_word..=max_word).collect::<Vec<_>>().chunks(250) {
                let mut multicall =
                    MulticallBuilder::new_dynamic(provider).address(multicall_address);
                for &word_pos in chunk {
                    word_pos_indices.push(word_pos);
                    multicall = multicall.add_dynamic(contract.tickTable(word_pos as i16));
                }
                let results = multicall.block(block_number).aggregate().await?;
                all_bitmaps.extend(results.into_iter().map(|tick_bitmap| tick_bitmap));
            }

            for (j, word_pos) in word_pos_indices.iter().enumerate() {
                let bitmap = all_bitmaps[j];

                if bitmap != U256::ZERO {
                    for i in 0..256 {
                        let bit = U256::from(1u64);
                        let initialized = (bitmap & (bit << i)) != U256::ZERO;

                        if initialized {
                            let tick_index = (word_pos * 256 + i as i32) * pool.tick_spacing;
                            tick_indices.push(tick_index);
                        }
                    }
                }
            }
        }
    }

    // Split tick fetching into chunks
    let mut all_ticks: BTreeMap<i32, Tick> = BTreeMap::new();
    match pool.pool_type {
        V3PoolType::UniswapV3 | V3PoolType::RamsesV2 | V3PoolType::PancakeV3 => {
            let contract = IUniswapV3Pool::new(pool.address, provider);
            for chunk in tick_indices.chunks(250) {
                let mut multicall =
                    MulticallBuilder::new_dynamic(provider).address(multicall_address);
                for &tick_index in chunk {
                    multicall = multicall.add_dynamic(
                        contract.ticks(Signed::<24, 1>::try_from(tick_index).unwrap()),
                    );
                }
                let results = multicall.block(block_number).aggregate().await?;
                for (i, tick_index) in chunk.iter().enumerate() {
                    let tick_response = &results[i];
                    let tick = Tick {
                        index: *tick_index,
                        liquidity_gross: tick_response.liquidityGross,
                        liquidity_net: tick_response.liquidityNet,
                    };
                    all_ticks.insert(*tick_index, tick);
                }
            }
        }
        V3PoolType::AlgebraV3
        | V3PoolType::AlgebraTwoSideFee
        | V3PoolType::AlgebraPoolFeeInState => {
            let contract = AlgebraV3Pool::new(pool.address, provider);
            for chunk in tick_indices.chunks(250) {
                let mut multicall =
                    MulticallBuilder::new_dynamic(provider).address(multicall_address);
                for &tick_index in chunk {
                    multicall = multicall.add_dynamic(
                        contract.ticks(Signed::<24, 1>::try_from(tick_index).unwrap()),
                    );
                }
                let results = multicall.block(block_number).aggregate().await?;
                for (i, tick_index) in chunk.iter().enumerate() {
                    let tick_response = &results[i];
                    let tick = Tick {
                        index: *tick_index,
                        liquidity_gross: tick_response.liquidityTotal.to::<U128>().to::<u128>(),
                        liquidity_net: tick_response.liquidityDelta,
                    };
                    all_ticks.insert(*tick_index, tick);
                }
            }
        }
    }

    pool.ticks = all_ticks;

    Ok(())
}

pub async fn calculate_ratio_conversion_factor<P: Provider + Send + Sync>(
    pool_v3: &UniswapV3Pool,
    provider: &Arc<P>,
    block_number: BlockId,
) -> Result<U256> {
    let quoter = get_ramses_quoter(pool_v3.factory);
    if let Some(quoter) = quoter {
        let quoter_instance = IQuoter::new(quoter, &provider);
        let amount_in = U256::from(100000000000u64);

        let ratio_conversion_factor_0 = match quoter_instance
            .quoteExactInputSingle(
                pool_v3.token0,
                pool_v3.token1,
                U24::from(pool_v3.fee),
                amount_in,
                Uint::from(0),
            )
            .call()
            .block(block_number)
            .await
        {
            Ok(amount_out_0) => {
                let amount_out_estimate_0 = pool_v3
                    .calculate_output(&pool_v3.token0, amount_in)
                    .unwrap();

                let ratio_conversion_factor_0 = if amount_out_estimate_0 == U256::ZERO {
                    U256::MAX
                } else if amount_out_0 == amount_out_estimate_0 {
                    U256::from(RAMSES_FACTOR)
                } else {
                    amount_out_0 * U256::from(RAMSES_FACTOR) / amount_out_estimate_0 - U256::ONE
                };
                info!("Ratio conversion factor 0: {}", ratio_conversion_factor_0);
                ratio_conversion_factor_0
            }
            Err(_) => {
                info!("Failed to fetch ratio conversion factor 0");
                U256::from(RAMSES_FACTOR)
            }
        };

        let ratio_conversion_factor_1 = match quoter_instance
            .quoteExactInputSingle(
                pool_v3.token1,
                pool_v3.token0,
                U24::from(pool_v3.fee),
                amount_in,
                Uint::from(0),
            )
            .call()
            .block(block_number)
            .await
        {
            Ok(amount_out_1) => {
                let amount_out_estimate_1 = pool_v3
                    .calculate_output(&pool_v3.token1, amount_in)
                    .unwrap();

                let ratio_conversion_factor_1 = if amount_out_estimate_1 == U256::ZERO {
                    U256::MAX
                } else if amount_out_1 == amount_out_estimate_1 {
                    U256::from(RAMSES_FACTOR)
                } else {
                    amount_out_1 * U256::from(RAMSES_FACTOR) / amount_out_estimate_1 - U256::ONE
                };
                info!("Ratio conversion factor 1: {}", ratio_conversion_factor_1);
                ratio_conversion_factor_1
            }
            Err(_) => {
                info!("Failed to fetch ratio conversion factor 1");
                U256::from(RAMSES_FACTOR)
            }
        };

        if ratio_conversion_factor_0 == U256::MAX && ratio_conversion_factor_1 == U256::MAX {
            Ok(U256::from(RAMSES_FACTOR))
        } else {
            Ok(ratio_conversion_factor_0.min(ratio_conversion_factor_1))
        }
    } else {
        Ok(U256::from(RAMSES_FACTOR))
    }
}
