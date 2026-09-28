//! Live checks of where V3 pools' fees come from, on Avalanche.
//!
//! ```bash
//! cargo test --features collector --test fee_source_live -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

mod common;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{address, aliases::U24, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::Result;
use std::sync::Arc;

use common::{CachingTokenInfo, CHAIN_ID, MULTICALL, RPC_URL};
use evm_dex_pool::collector::read_fees;
use evm_dex_pool::v3::{fetch_v3_pool, FeeSource, UniswapV3Pool};
use evm_dex_pool::{PoolInterface, PoolRegistry};

/// Historical state needs an archive node; the public endpoint prunes it.
const ARCHIVE_RPC_URL: &str = "https://avalanche.rpc.sentio.xyz";

/// Ramses-family pool that swaps at `currentFee()` = 75 while `fee()` reads
/// 50. Run 8 priced it at 50 and reverted (bulk_001#32).
const CURRENT_FEE_POOL: Address = address!("0x0021368b76e7F280accAd186ae06039EB1d499b8");
const USDC: Address = address!("0xB97EF9Ef8734C71904D8002F8b6Bc66Dd9c48a6E");

/// At block 96,191,167 a 42,910,915 USDC swap through the pool paid out
/// 42,916,873 AUSD on a fork of that block.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn a_current_fee_pool_is_priced_at_its_current_fee() -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(ARCHIVE_RPC_URL.parse()?));
    let pool = fetch_v3_pool(
        &provider,
        CURRENT_FEE_POOL,
        BlockId::Number(BlockNumberOrTag::Number(96_191_167)),
        &CachingTokenInfo::new(),
        MULTICALL,
        CHAIN_ID,
    )
    .await?;
    assert_eq!(pool.fee, U24::from(75u32));
    assert_eq!(pool.fee_source, FeeSource::ReadCurrentFee);
    assert_eq!(
        pool.calculate_output(&USDC, U256::from(42_910_915u64))?,
        U256::from(42_916_873u64)
    );
    Ok(())
}

/// Avalanche Algebra pool with `DYNAMIC_FEE` off: its fee is event-driven.
const ALGEBRA_STATIC_FEE: Address = address!("0x23fF0B5370BF33725918e6105108f3fa2c4b8a05");

/// A pool restored from a snapshot comes back `Unknown`; one read classifies
/// it. The Algebra pool turns out event-driven and is no longer tracked; the
/// `currentFee()` pool stays tracked at its current fee.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn one_read_classifies_restored_pools() -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let block = provider.get_block_number().await?;
    let registry = Arc::new(PoolRegistry::new(CHAIN_ID));
    for address in [ALGEBRA_STATIC_FEE, CURRENT_FEE_POOL] {
        let mut pool = fetch_v3_pool(
            &provider,
            address,
            BlockId::Number(BlockNumberOrTag::Number(block)),
            &CachingTokenInfo::new(),
            MULTICALL,
            CHAIN_ID,
        )
        .await?;
        pool.fee_source = FeeSource::Unknown; // what a snapshot restore gives
        registry.add_pool(Box::new(pool));
    }
    registry.set_last_processed_block(block);
    assert_eq!(registry.get_dynamic_fee_addresses().len(), 2);

    read_fees(&provider, &registry, &[ALGEBRA_STATIC_FEE, CURRENT_FEE_POOL], MULTICALL, CHAIN_ID)
        .await?;

    assert_eq!(registry.get_dynamic_fee_addresses(), vec![CURRENT_FEE_POOL]);
    for (address, want) in [
        (ALGEBRA_STATIC_FEE, FeeSource::Events),
        (CURRENT_FEE_POOL, FeeSource::ReadCurrentFee),
    ] {
        let pool = registry.get_pool(&address).unwrap();
        let guard = pool.read().await;
        let v3 = guard.as_any().downcast_ref::<UniswapV3Pool>().unwrap();
        assert_eq!(v3.fee_source, want, "{address}");
    }
    Ok(())
}
