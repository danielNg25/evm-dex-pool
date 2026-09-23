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
use alloy::primitives::{address, Address};
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::Result;
use std::sync::Arc;

use common::{CachingTokenInfo, CHAIN_ID, MULTICALL, RPC_URL};
use evm_dex_pool::v3::{fetch_v3_pool, V3PoolType};

/// Pharaoh (Ramses CL fork). `lastPeriod()` answers 2959; `globalState()` reverts.
const PHARAOH: Address = address!("0xFf0855A9027f5F5c2bbaCC4aAC477AfbeeefbeA9");
/// Plain Uniswap V3. Both `lastPeriod()` and `globalState()` revert.
const UNISWAP_V3: Address = address!("0x7b602f98D71715916E7c963f51bfEbC754aDE2d0");
/// Algebra. `globalState()` answers; `lastPeriod()` reverts.
const ALGEBRA: Address = address!("0x23fF0B5370BF33725918e6105108f3fa2c4b8a05");
/// The one configured pool deployed by the factory that used to be hardcoded
/// in `RAMSES_FACTORIES`. It answers `lastPeriod()` (2959) like any other
/// Ramses-family pool, but the factory allowlist claimed it first and pinned
/// it to `RamsesV2`, which excluded it from the mutable-fee refetch set.
const FORMER_RAMSES_V2: Address = address!("0x0021368b76e7F280accAd186ae06039EB1d499b8");

async fn classify(address: Address) -> Result<V3PoolType> {
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
        "[test] {address} -> {:?} (fee {})",
        pool.pool_type, pool.fee
    );
    Ok(pool.pool_type)
}

/// The Pharaoh pool must classify as `RamsesCL`, which is what puts it on
/// the mutable-fee refetch list.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pharaoh_pool_classifies_as_ramses_cl() -> Result<()> {
    assert_eq!(classify(PHARAOH).await?, V3PoolType::RamsesCL);
    Ok(())
}

/// Regression guard: `lastPeriod()` must not widen onto plain Uniswap V3.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn uniswap_v3_pool_stays_uniswap_v3() -> Result<()> {
    assert_eq!(classify(UNISWAP_V3).await?, V3PoolType::UniswapV3);
    Ok(())
}

/// Regression guard: the new check runs only after the existing chain, so an
/// Algebra pool keeps its classification.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn algebra_pool_stays_algebra() -> Result<()> {
    assert_eq!(classify(ALGEBRA).await?, V3PoolType::AlgebraV3);
    Ok(())
}

/// Regression guard for removing the `ratio_conversion_factor` calibration.
/// With the factory allowlist gone, this pool falls through to the
/// `lastPeriod()` check like every other Ramses-family pool and classifies as
/// `RamsesCL` -- which is what puts it on the fee-refetch list. Previously it
/// was `RamsesV2` and got a constant calibrated once at startup instead.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn former_ramses_v2_pool_now_classifies_as_ramses_cl() -> Result<()> {
    assert_eq!(classify(FORMER_RAMSES_V2).await?, V3PoolType::RamsesCL);
    Ok(())
}
