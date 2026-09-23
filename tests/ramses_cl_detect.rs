//! Live classification check for Ramses-family CL pools on Avalanche.
//!
//! Ramses-family CL forks (Pharaoh here, also Shadow/Nile/Cleo elsewhere) are
//! Uniswap V3-shaped: they keep `slot0()`, so the Algebra discriminator
//! `globalState()` is blind to them and they used to fall through as
//! `V3PoolType::UniswapV3` with a fee frozen at startup. `lastPeriod()` is the
//! marker that separates them.
//!
//! ```bash
//! cargo test --features collector --test ramses_cl_detect -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

mod common;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{address, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::Result;
use std::sync::Arc;

use common::{CachingTokenInfo, CHAIN_ID, MULTICALL, RPC_URL};
use evm_dex_pool::v3::{fetch_v3_pool, V3PoolType, RAMSES_FACTOR};

/// Pharaoh (Ramses CL fork). `lastPeriod()` answers 2959; `globalState()` reverts.
const PHARAOH: Address = address!("0xFf0855A9027f5F5c2bbaCC4aAC477AfbeeefbeA9");
/// Plain Uniswap V3. Both `lastPeriod()` and `globalState()` revert.
const UNISWAP_V3: Address = address!("0x7b602f98D71715916E7c963f51bfEbC754aDE2d0");
/// Algebra. `globalState()` answers; `lastPeriod()` reverts.
const ALGEBRA: Address = address!("0x23fF0B5370BF33725918e6105108f3fa2c4b8a05");

async fn classify(address: Address) -> Result<(V3PoolType, U256)> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let block = provider.get_block_number().await?;
    let pool = fetch_v3_pool(
        &provider,
        address,
        BlockId::Number(BlockNumberOrTag::Number(block)),
        &CachingTokenInfo::new(),
        MULTICALL,
        CHAIN_ID,
    )
    .await?;
    println!(
        "[test] {address} -> {:?} (fee {}, ratio_conversion_factor {})",
        pool.pool_type, pool.fee, pool.ratio_conversion_factor
    );
    Ok((pool.pool_type, pool.ratio_conversion_factor))
}

/// The Pharaoh pool must classify as `RamsesCL` and must NOT pick up a
/// calibrated `ratio_conversion_factor` -- its factory is absent from
/// `RAMSES_FACTORIES`, so there is no quoter to calibrate against and the
/// `RamsesV2` scaling would be wrong.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pharaoh_pool_classifies_as_ramses_cl() -> Result<()> {
    let (pool_type, factor) = classify(PHARAOH).await?;
    assert_eq!(pool_type, V3PoolType::RamsesCL);
    assert_eq!(
        factor,
        U256::from(RAMSES_FACTOR),
        "RamsesCL must keep the identity ratio conversion factor"
    );
    Ok(())
}

/// Regression guard: `lastPeriod()` must not widen onto plain Uniswap V3.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn uniswap_v3_pool_stays_uniswap_v3() -> Result<()> {
    let (pool_type, _) = classify(UNISWAP_V3).await?;
    assert_eq!(pool_type, V3PoolType::UniswapV3);
    Ok(())
}

/// Regression guard: the new check runs only after the existing chain, so an
/// Algebra pool keeps its classification.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn algebra_pool_stays_algebra() -> Result<()> {
    let (pool_type, _) = classify(ALGEBRA).await?;
    assert_eq!(pool_type, V3PoolType::AlgebraV3);
    Ok(())
}
