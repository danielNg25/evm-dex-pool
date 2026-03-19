//! Integration tests for graceful error handling when V3 pool fetching fails.
//!
//! Reproduces the exact bug.txt scenario on Katana network (chain 747474):
//! pool watcher detected pool 0xE5262F14850f8AC9A38D86B01088fAf3E41930ec,
//! `fetch_v3_pool` panicked at `multicall_result.4.unwrap()` because `slot0()`
//! returned `Failure { idx: 4, return_data: 0x }`.
//!
//! ```bash
//! cargo test --features collector --test v3_fetch_error -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{address, Address};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use anyhow::Result;

use evm_dex_pool::collector::{
    resolve_multicall_address, start_collector, CollectorConfig, PoolFetchConfig,
};
use evm_dex_pool::v3::fetch_v3_pool;
use evm_dex_pool::{PoolRegistry, TokenInfo};

// ---------------------------------------------------------------------------
// ERC-20 decimals() ABI
// ---------------------------------------------------------------------------
sol! {
    #[sol(rpc)]
    interface IERC20 {
        function decimals() external view returns (uint8);
    }
}

// ---------------------------------------------------------------------------
// SimpleTokenCache
// ---------------------------------------------------------------------------
struct SimpleTokenCache {
    cache: Arc<Mutex<HashMap<Address, (Address, u8)>>>,
}

impl SimpleTokenCache {
    fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl TokenInfo for SimpleTokenCache {
    fn get_or_fetch_token<P: Provider + Send + Sync>(
        &self,
        provider: &Arc<P>,
        address: Address,
        _multicall_address: Address,
    ) -> impl std::future::Future<Output = anyhow::Result<(Address, u8)>> + Send {
        let cached = self.cache.lock().unwrap().get(&address).copied();
        let cache = Arc::clone(&self.cache);
        let provider = Arc::clone(provider);

        async move {
            if let Some(entry) = cached {
                return Ok(entry);
            }
            let contract = IERC20::new(address, &provider);
            let decimals = contract.decimals().call().await?;
            cache.lock().unwrap().insert(address, (address, decimals));
            Ok((address, decimals))
        }
    }
}

// ---------------------------------------------------------------------------
// Constants — exact values from bug.txt (Katana network, chain 747474)
// ---------------------------------------------------------------------------
const CHAIN_ID: u64 = 747474;
const KATANA_RPC: &str = "https://katana.gateway.tenderly.co/5tSBKWoYZTBaxrNpUvED7H";
const KATANA_WS: &str = "wss://katana.gateway.tenderly.co/5tSBKWoYZTBaxrNpUvED7H";

/// The exact pool address from bug.txt that caused the panic.
/// `slot0()` returns `Failure { idx: 4, return_data: 0x }` on this pool.
const BAD_POOL: Address = address!("0xE5262F14850f8AC9A38D86B01088fAf3E41930ec");

fn build_fetch_config() -> PoolFetchConfig {
    PoolFetchConfig {
        chain_id: CHAIN_ID,
        multicall_address: None,
        factory_to_fee: HashMap::new(),
        aero_factory_addresses: vec![],
        chunk_size: 5,
        wait_time_between_chunks: 200,
        max_retries: 1,
        parallel_fetch: true,
    }
}

// ---------------------------------------------------------------------------
// Test A — fetch_v3_pool returns Err on the exact bug.txt pool
// ---------------------------------------------------------------------------

/// Reproduces bug.txt exactly: calls `fetch_v3_pool` on pool
/// 0xE5262F14850f8AC9A38D86B01088fAf3E41930ec on Katana (chain 747474).
///
/// Before the fix: panics with `Failure { idx: 4, return_data: 0x }`.
/// After the fix: returns `Err`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_fetch_v3_pool_returns_error_on_bug_pool() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();

    let provider = Arc::new(ProviderBuilder::new().connect_http(KATANA_RPC.parse()?));
    let token_info = SimpleTokenCache::new();
    let block = provider.get_block_number().await?;
    let multicall_address = resolve_multicall_address(CHAIN_ID, None);

    println!(
        "[test] Fetching bug.txt pool {} on Katana at block {}",
        BAD_POOL, block
    );

    let result = fetch_v3_pool(
        &provider,
        BAD_POOL,
        BlockId::Number(BlockNumberOrTag::Number(block)),
        &token_info,
        multicall_address,
        CHAIN_ID,
    )
    .await;

    assert!(
        result.is_err(),
        "Expected Err when fetching bug.txt pool as V3, got Ok"
    );
    println!(
        "[test] fetch_v3_pool correctly returned error: {}",
        result.unwrap_err()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Test B — collector survives a failed add_pools (WS mode, same as bug.txt)
// ---------------------------------------------------------------------------

/// End-to-end reproduction of bug.txt: start a collector in WebSocket mode
/// on Katana, then call `add_pools` with the bad pool address. The collector
/// must return Err and keep running — not crash the tokio runtime.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_add_pools_survives_bad_pool_katana() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();

    let provider = Arc::new(ProviderBuilder::new().connect_http(KATANA_RPC.parse()?));
    let token_info = SimpleTokenCache::new();
    let fetch_config = build_fetch_config();
    let registry = Arc::new(PoolRegistry::new(CHAIN_ID));

    // ── Bootstrap — need at least one good pool on Katana ──────────────────
    // Use identify_pool_type + fetch to find a working pool on Katana.
    // For now, bootstrap with an empty registry and just test that add_pools
    // with the bad pool doesn't crash the collector.
    let start_block = provider.get_block_number().await?;
    println!("[test] start_block = {start_block}");

    // ── Start collector in WS mode (same as bug.txt) ───────────────────────
    let mut handle = start_collector(
        Arc::clone(&provider),
        CollectorConfig {
            start_block,
            max_blocks_per_batch: 5,
            use_pending_blocks: false,
            use_websocket: true,
            websocket_urls: vec![KATANA_WS.to_string()],
            wait_time: 2_000,
        },
        Arc::clone(&registry),
        None,
        None,
    )
    .await?;

    println!("[test] collector running in WS mode — waiting 10s …");
    tokio::time::sleep(Duration::from_secs(10)).await;

    // ── Try to add the exact bad pool from bug.txt ─────────────────────────
    println!(
        "[test] calling add_pools with bug.txt pool {} …",
        BAD_POOL
    );
    let add_result = handle
        .add_pools(vec![BAD_POOL], start_block, &fetch_config, &token_info)
        .await;

    assert!(
        add_result.is_err(),
        "Expected add_pools to return Err for bug.txt pool"
    );
    println!(
        "[test] add_pools correctly returned error: {}",
        add_result.unwrap_err()
    );

    // ── Verify collector is still alive ────────────────────────────────────
    println!("[test] verifying collector survived …");
    tokio::time::sleep(Duration::from_secs(5)).await;

    // The collector should still be operational (not panicked).
    // With an empty initial registry we just verify it stops cleanly.
    handle.stop().await;
    println!("[test] collector stopped cleanly — ══ PASSED ══");
    Ok(())
}
