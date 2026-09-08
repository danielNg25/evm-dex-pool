//! Live version-detection tests against known Avalanche LB pairs.
//!
//! ```bash
//! cargo test --features collector test_detect -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

use std::sync::Arc;

use alloy::eips::BlockId;
use alloy::primitives::{address, Address};
use alloy::providers::ProviderBuilder;
use anyhow::Result;

use evm_dex_pool::collector::identify_pool_type;
use evm_dex_pool::lb::{detect_lb_version, LBVersion};
use evm_dex_pool::PoolType;

const RPC_URL: &str = "https://api.avax.network/ext/bc/C/rpc";
const MULTICALL: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

const V22_POOL: Address = address!("8573f98175d816d520248b5facf40d309b1c9cee");
const V21_POOL: Address = address!("4224f6f4c9280509724db2dbac314621e4465c29");
// LB v2.0 WAVAX/USDC on Avalanche.
const V20_POOL: Address = address!("18332988456C4Bd9ABa6698ec748b331516F5A14");
// A non-LB pair, to prove detection returns None rather than guessing.
const V2_PAIR: Address = address!("f4003F4efBE8691B60249E6afbD307aBE7758adb");

#[tokio::test]
#[ignore]
async fn test_detect_lb_version() -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let block = BlockId::latest();

    for (addr, expected) in [
        (V22_POOL, Some(LBVersion::V2_2)),
        (V21_POOL, Some(LBVersion::V2_1)),
        (V20_POOL, Some(LBVersion::V2_0)),
        (V2_PAIR, None),
    ] {
        let got = detect_lb_version(&provider, addr, MULTICALL, block).await?;
        assert_eq!(got, expected, "version mismatch for {addr}");
    }
    Ok(())
}

/// The regression this whole task exists for: a v2.0 pair must not be
/// classified as UniswapV2, which would panic the bootstrap.
#[tokio::test]
#[ignore]
async fn test_v20_pair_is_not_misclassified() -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let got = identify_pool_type(&provider, V20_POOL, MULTICALL).await?;
    assert_eq!(got, PoolType::TraderJoeLB);
    Ok(())
}

use evm_dex_pool::lb::fetch_lb_pool;
use evm_dex_pool::TokenInfo;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;

struct NoopTokenInfo;

impl TokenInfo for NoopTokenInfo {
    fn get_or_fetch_token<P: alloy::providers::Provider + Send + Sync>(
        &self,
        _provider: &Arc<P>,
        address: Address,
        _multicall_address: Address,
    ) -> impl Future<Output = Result<(Address, u8)>> + Send {
        async move { Ok((address, 18u8)) }
    }
}

#[tokio::test]
#[ignore]
async fn test_fetch_stamps_version_and_finds_bins() -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let block = BlockId::latest();

    for (addr, expected) in [(V22_POOL, LBVersion::V2_2), (V21_POOL, LBVersion::V2_1)] {
        let pool = fetch_lb_pool(&provider, addr, block, &NoopTokenInfo, MULTICALL, 43114).await?;

        assert_eq!(pool.version, expected, "version not stamped for {addr}");
        // A live WAVAX pair always has far more than one non-empty bin. One
        // bin means bin discovery silently degraded to the walk fallback.
        assert!(
            pool.bins.len() > 1,
            "{addr}: only {} bin(s) discovered — discovery degraded",
            pool.bins.len()
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_fetch_v20_pool() -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let pool = fetch_lb_pool(
        &provider,
        V20_POOL,
        BlockId::latest(),
        &NoopTokenInfo,
        MULTICALL,
        43114,
    )
    .await?;

    assert_eq!(pool.version, LBVersion::V2_0);
    assert!(pool.bin_step > 0, "bin_step must come from feeParameters");
    assert!(
        pool.active_id > 0,
        "active_id must come from getReservesAndId"
    );
    assert!(
        pool.bins.len() > 1,
        "expected multiple non-empty bins, got {}",
        pool.bins.len()
    );
    assert!(pool.hooks_parameters.is_none(), "v2.0 has no hooks");
    Ok(())
}
