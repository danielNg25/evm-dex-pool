//! Live checks of where V3 pools' fees come from, on Avalanche.
//!
//! ```bash
//! cargo test --features collector --test fee_source_live -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

mod common;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{address, aliases::U24, Address, U256};
use alloy::providers::ProviderBuilder;
use anyhow::Result;
use std::sync::Arc;

use common::{CachingTokenInfo, CHAIN_ID, MULTICALL};
use evm_dex_pool::v3::{fetch_v3_pool, FeeSource};
use evm_dex_pool::PoolInterface;

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
