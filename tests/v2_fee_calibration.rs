//! Live fee resolution for the Avalanche factory that broke the heuristic.
//!
//! `0x85448bf2` reports `fee()` in the same 1e6 basis this crate stores, which
//! the magnitude heuristic in `fetch_v2_pool` could not see. All three of its
//! configured pools were mispriced at once:
//!
//!   0x0afdee81  fee() 15000  ->  wrapped to ~U256::MAX, panicked every quote
//!   0x1b7bcd44  fee() 15000  ->  same
//!   0x903c3ed1  fee()  5000  ->  priced at 50%, silently, for a 42-hour run
//!
//! The true fees, back-solved from each pool's own `getAmountOut`, are 1.5%,
//! 1.5% and 0.5%.
//!
//! ```bash
//! cargo test --features collector --test v2_fee_calibration -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

mod common;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{address, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::Result;
use std::sync::Arc;

use common::{CachingTokenInfo, CHAIN_ID, MULTICALL, RPC_URL};
use evm_dex_pool::v2::fetch_v2_pool;

use std::collections::HashMap;

/// 1.5%, reported as 15000. Wrapped before the fix.
const PHARAOH_WAVAX: Address = address!("0x0afdEE8162CCeAd9AC6a30C94F691E6e7d1Af670");
/// 1.5%, reported as 15000. Wrapped before the fix.
const PHARAOH_USDC: Address = address!("0x1b7BCd44e77adBB31bEa105842139D78F352dc81");
/// 0.5%, reported as 5000. Read as 50% before the fix -- no panic, just wrong.
const PHARAOH_HALF_PCT: Address = address!("0x903C3ED10D21bDF9d2a04d6548aa13477b4F869c");

async fn resolved_fee(pool: Address) -> Result<U256> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let block = provider.get_block_number().await?;
    let pool = fetch_v2_pool(
        &provider,
        pool,
        BlockId::Number(BlockNumberOrTag::Number(block)),
        &CachingTokenInfo::new(),
        MULTICALL,
        CHAIN_ID,
        &HashMap::new(),
        &[],
    )
    .await?;
    println!("[test] {} -> fee {}", pool.address, pool.fee);
    Ok(pool.fee)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn a_pool_reporting_its_fee_in_the_stored_basis_is_not_wrapped() -> Result<()> {
    for pool in [PHARAOH_WAVAX, PHARAOH_USDC] {
        let fee = resolved_fee(pool).await?;
        assert_eq!(fee, U256::from(15_000), "{pool} should resolve to 1.5%");
    }
    Ok(())
}

/// The quiet one. It never panicked, so nothing pointed at it: the heuristic
/// read 5000 as a 1e4-basis fee and multiplied it to 500000, pricing a 0.5%
/// pool at 50%.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn an_ambiguous_reading_is_calibrated_rather_than_multiplied() -> Result<()> {
    let fee = resolved_fee(PHARAOH_HALF_PCT).await?;
    assert_eq!(fee, U256::from(5_000), "should resolve to 0.5%, not 50%");
    Ok(())
}
