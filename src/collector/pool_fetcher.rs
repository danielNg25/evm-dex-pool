use crate::contracts_rpc::RpcIUniswapV3Pool as IUniswapV3Pool;
use crate::erc4626::fetch_erc4626_pool;
use crate::pool::base::PoolInterface;
use crate::v2::fetch_v2_pool;
use crate::v3::fetch_v3_pool;
use crate::{PoolRegistry, PoolType, TokenInfo};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::Address;
use alloy::providers::Provider;
use anyhow::Result;
use futures_util::future::join_all;
use log::{debug, error, info};
use std::collections::HashSet;
use std::sync::Arc;

use super::config::PoolFetchConfig;

/// Detect whether a pool is V2 or V3 by calling `liquidity()` (V3-specific).
pub async fn identify_pool_type<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pool_address: Address,
) -> Result<PoolType> {
    let pair_instance = IUniswapV3Pool::new(pool_address, provider);
    let fee_call = pair_instance.liquidity().into_transaction_request();

    match provider.call(fee_call).await {
        Ok(_) => Ok(PoolType::UniswapV3),
        Err(_) => Ok(PoolType::UniswapV2),
    }
}

/// Fetch a single pool by type using the appropriate fetcher.
pub async fn fetch_pool<P: Provider + Send + Sync, T: TokenInfo>(
    provider: &Arc<P>,
    pool_address: Address,
    block_number: BlockId,
    pool_type: PoolType,
    token_info: &T,
    config: &PoolFetchConfig,
) -> Result<Box<dyn PoolInterface>> {
    match pool_type {
        PoolType::UniswapV2 => {
            let pool = fetch_v2_pool(
                provider,
                pool_address,
                block_number,
                token_info,
                config.multicall_address,
                config.chain_id,
                &config.factory_to_fee,
                &config.aero_factory_addresses,
            )
            .await?;
            Ok(Box::new(pool))
        }
        PoolType::UniswapV3 => {
            let pool = fetch_v3_pool(
                provider,
                pool_address,
                block_number,
                token_info,
                config.multicall_address,
                config.chain_id,
            )
            .await?;
            Ok(Box::new(pool))
        }
        PoolType::ERC4626(pool_type) => {
            let pool = fetch_erc4626_pool(
                provider,
                pool_type,
                pool_address,
                block_number,
                token_info,
            )
            .await?;
            Ok(pool)
        }
    }
}

/// Fetch pools from chain into the registry.
///
/// Skips pools already present in the registry. Fetches in parallel chunks
/// with exponential backoff retry. Registers event topics for fetched pool types.
///
/// Returns addresses of newly fetched pools (so callers can do app-specific work).
pub async fn fetch_pools_into_registry<P: Provider + Send + Sync, T: TokenInfo>(
    provider: &Arc<P>,
    pool_addresses: &[Address],
    block_number: BlockNumberOrTag,
    token_info: &T,
    pool_registry: &Arc<PoolRegistry>,
    config: &PoolFetchConfig,
) -> Result<Vec<Address>> {
    info!(
        "[Chain {}] Starting pool fetch at block: {}",
        config.chain_id,
        block_number.as_number().unwrap()
    );

    // Filter out pools already in registry
    let mut new_pool_addresses = Vec::new();
    for &address in pool_addresses {
        if pool_registry.get_pool(&address).is_some() {
            debug!("[Chain {}] Pool {} already exists in registry, skipping", config.chain_id, address);
        } else {
            new_pool_addresses.push(address);
        }
    }

    info!(
        "[Chain {}] Fetching {} new pools in parallel chunks",
        config.chain_id,
        new_pool_addresses.len()
    );

    let chunk_size = config.chunk_size.max(1);
    let chunk_count = (new_pool_addresses.len() + chunk_size - 1) / chunk_size;
    let mut pool_types_present = HashSet::new();
    let mut fetched_addresses = Vec::new();

    for (chunk_idx, chunk) in new_pool_addresses.chunks(chunk_size).enumerate() {
        info!(
            "[Chain {}] Fetching chunk {}/{} ({} pools)",
            config.chain_id,
            chunk_idx + 1,
            chunk_count,
            chunk.len()
        );

        let futures: Vec<_> = chunk
            .iter()
            .map(|&address| {
                let provider = provider.clone();
                async move {
                    let pool_type = identify_pool_type(&provider, address).await?;
                    let pool = fetch_pool(
                        &provider,
                        address,
                        BlockId::Number(block_number),
                        pool_type,
                        token_info,
                        config,
                    )
                    .await?;
                    Ok::<_, anyhow::Error>((address, pool_type, pool))
                }
            })
            .collect();

        let results = join_all(futures).await;

        let mut failed_pools: Vec<Address> = Vec::new();

        for (i, result) in results.into_iter().enumerate() {
            match result {
                Ok((address, pool_type, pool)) => {
                    info!("[Chain {}] Fetched pool {} ({:?})", config.chain_id, address, pool_type);
                    pool_registry.add_pool(pool);
                    pool_types_present.insert(pool_type);
                    fetched_addresses.push(address);
                }
                Err(e) => {
                    error!("[Chain {}] Failed to fetch pool {}: {}", config.chain_id, chunk[i], e);
                    failed_pools.push(chunk[i]);
                }
            }
        }

        // Retry failed pools with exponential backoff
        for &address in &failed_pools {
            let mut success = false;
            for attempt in 1..=config.max_retries {
                let delay =
                    tokio::time::Duration::from_millis(500 * 2u64.pow(attempt - 1));
                info!(
                    "[Chain {}] Retrying pool {} (attempt {}/{}) after {:?}",
                    config.chain_id, address, attempt, config.max_retries, delay
                );
                tokio::time::sleep(delay).await;

                match async {
                    let pool_type = identify_pool_type(provider, address).await?;
                    let pool = fetch_pool(
                        provider,
                        address,
                        BlockId::Number(block_number),
                        pool_type,
                        token_info,
                        config,
                    )
                    .await?;
                    Ok::<_, anyhow::Error>((pool_type, pool))
                }
                .await
                {
                    Ok((pool_type, pool)) => {
                        info!(
                            "[Chain {}] Fetched pool {} ({:?}) on retry {}",
                            config.chain_id, address, pool_type, attempt
                        );
                        pool_registry.add_pool(pool);
                        pool_types_present.insert(pool_type);
                        fetched_addresses.push(address);
                        success = true;
                        break;
                    }
                    Err(e) => {
                        error!(
                            "[Chain {}] Retry {}/{} failed for pool {}: {}",
                            config.chain_id, attempt, config.max_retries, address, e
                        );
                    }
                }
            }
            if !success {
                panic!(
                    "Failed to fetch pool {} after {} retries. Check RPC connection and rate limits.",
                    address, config.max_retries
                );
            }
        }

        // Sleep between chunks to respect rate limits
        if chunk_idx < chunk_count - 1 {
            info!(
                "[Chain {}] Waiting {} milliseconds before next chunk...",
                config.chain_id, config.wait_time_between_chunks
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(
                config.wait_time_between_chunks,
            ))
            .await;
        }
    }

    // Set last processed block
    pool_registry.set_last_processed_block(block_number.as_number().unwrap());

    // Register event topics for all pool types found
    for pool_type in pool_types_present {
        pool_registry.add_topics(pool_type.topics());
        pool_registry.add_profitable_topics(pool_type.profitable_topics());
    }

    Ok(fetched_addresses)
}
