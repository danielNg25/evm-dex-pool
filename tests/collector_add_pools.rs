//! Integration tests for `CollectorHandle::add_pools`.
//!
//! These tests hit live Ethereum mainnet RPC endpoints and are therefore
//! marked `#[ignore]`.  Run them explicitly with:
//!
//! ```bash
//! cargo test --features collector -- --ignored --nocapture 2>&1 | tee test.log
//! # or individually:
//! cargo test --features collector test_add_pools_http -- --ignored --nocapture
//! cargo test --features collector test_add_pools_ws   -- --ignored --nocapture
//! ```
//!
//! What each test verifies:
//! * **HTTP (LatestBlock) test** — exact state comparison: after stopping the
//!   collector every pool's `calculate_output` must match a freshly-fetched
//!   reference at the same `last_processed_block`.
//! * **WS test** — sanity check: all pools (initial + newly added) must exist
//!   in the registry and return non-zero output.  An exact block comparison is
//!   not possible in WS mode because `last_processed_block` is only advanced
//!   during the bootstrap phase, not during live streaming.

#![cfg(feature = "collector")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{address, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use anyhow::Result;

use evm_dex_pool::collector::{
    fetch_pool, fetch_pools_into_registry, identify_pool_type, start_collector, CollectorConfig,
    PoolFetchConfig,
};
use evm_dex_pool::{PoolRegistry, TokenInfo};

// ---------------------------------------------------------------------------
// ERC-20 decimals() ABI (inline, no JSON file needed)
// ---------------------------------------------------------------------------
sol! {
    #[sol(rpc)]
    interface IERC20 {
        function decimals() external view returns (uint8);
    }
}

// ---------------------------------------------------------------------------
// SimpleTokenCache — minimal TokenInfo impl backed by a thread-safe HashMap
// ---------------------------------------------------------------------------

/// A lightweight `TokenInfo` implementation for tests.
///
/// Caches (address → (address, decimals)) to avoid redundant RPC calls.
/// Uses `Arc<Mutex<…>>` so the `Arc` can be cheaply cloned into the async
/// future without capturing `&self` across await points.
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
        // Extract cached value (while holding the lock briefly) then drop the
        // guard before entering the async block so no non-Send guard crosses
        // an await point.
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
// Helpers
// ---------------------------------------------------------------------------

const CHAIN_ID: u64 = 1; // Base mainnet
const HTTP_RPC: &str = "https://eth-mainnet.public.blastapi.io";
const WS_RPCS: &[&str] = &["wss://ethereum-rpc.publicnode.com", "wss://eth.drpc.org"];

fn build_fetch_config() -> PoolFetchConfig {
    PoolFetchConfig {
        chain_id: CHAIN_ID,
        multicall_address: None,
        factory_to_fee: HashMap::new(),
        aero_factory_addresses: vec![],
        chunk_size: 5,
        wait_time_between_chunks: 200,
        max_retries: 3,
        parallel_fetch: true,
    }
}

/// Verify `addresses` exist in the registry and that each pool's
/// `calculate_output` matches a freshly-fetched on-chain reference at
/// `at_block`.
///
/// Panics with a clear message on any mismatch.
async fn assert_pools_match_chain_at<P>(
    provider: &Arc<P>,
    registry: &Arc<PoolRegistry>,
    fetch_config: &PoolFetchConfig,
    token_info: &SimpleTokenCache,
    addresses: &[Address],
    at_block: u64,
) where
    P: Provider + Send + Sync + Clone + 'static,
{
    println!(
        "[verify] Checking {} pool(s) at block {}",
        addresses.len(),
        at_block
    );
    // 1 ETH worth of token0 as a representative swap amount
    let test_amount = U256::from(1_000_000_000_000_000_000_u128);

    for &addr in addresses {
        let pool_arc = registry
            .get_pool(&addr)
            .unwrap_or_else(|| panic!("[verify] Pool {} not found in registry", addr));
        let pool = pool_arc.read().await;

        let pool_type = identify_pool_type(provider, addr)
            .await
            .unwrap_or_else(|e| panic!("[verify] identify_pool_type failed for {addr}: {e}"));

        let ref_pool = fetch_pool(
            provider,
            addr,
            BlockId::Number(BlockNumberOrTag::Number(at_block)),
            pool_type,
            token_info,
            fetch_config,
        )
        .await
        .unwrap_or_else(|e| panic!("[verify] fetch_pool failed for {addr}: {e}"));

        let (token0, _) = pool.tokens();
        let registry_out = pool
            .calculate_output(&token0, test_amount)
            .unwrap_or(U256::ZERO);
        let ref_out = ref_pool
            .calculate_output(&token0, test_amount)
            .unwrap_or(U256::ZERO);

        let status = if registry_out == ref_out {
            "OK"
        } else {
            "MISMATCH"
        };
        println!("  [{status}] pool={addr}  registry={registry_out}  ref={ref_out}");

        assert_eq!(
            registry_out, ref_out,
            "Pool {addr} output mismatch at block {at_block}: registry={registry_out} != ref={ref_out}"
        );
    }
}

/// Light sanity check — every pool exists in the registry and produces
/// non-zero output.  Used where an exact block comparison is not possible
/// (WS streaming mode).
async fn assert_pools_valid(registry: &Arc<PoolRegistry>, addresses: &[Address]) {
    let test_amount = U256::from(1_000_000_000_000_000_000_u128);

    for &addr in addresses {
        let pool_arc = registry
            .get_pool(&addr)
            .unwrap_or_else(|| panic!("[sanity] Pool {} not found in registry", addr));
        let pool = pool_arc.read().await;
        let (token0, _) = pool.tokens();
        let out = pool
            .calculate_output(&token0, test_amount)
            .unwrap_or(U256::ZERO);
        println!("  [sanity] pool={addr}  output={out}");
        assert!(
            out > U256::ZERO,
            "Pool {addr} returned zero output — state may be corrupted"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 1 — HTTP (LatestBlock) mode
// ---------------------------------------------------------------------------

/// End-to-end HTTP mode test:
///
/// 1. Bootstrap the registry with 3 initial Ethereum mainnet pools.
/// 2. Start the collector in LatestBlock mode.
/// 3. Wait 30 s so the collector processes a few live blocks.
/// 4. Call `handle.add_pools` with 2 new pool addresses (collector keeps
///    running during the slow RPC fetch phase).
/// 5. Wait 30 s more.
/// 6. Stop the collector.
/// 7. Re-fetch all 5 pools from the chain at `last_processed_block` and
///    assert exact output equality with the registry.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_add_pools_http() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();

    let provider = Arc::new(ProviderBuilder::new().connect_http(HTTP_RPC.parse()?));

    // Initial pools (Base mainnet)
    let initial_addrs = vec![
        address!("0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640"),
        address!("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc"),
    ];
    // Pools to add dynamically
    let new_addrs = vec![
        address!("0xa43fe16908251ee70ef74718545e4fe6c5ccec9f"),
        address!("0x4e68ccd3e89f51c3074ca5072bbac773960dfa36"),
    ];
    let all_addrs: Vec<Address> = initial_addrs
        .iter()
        .chain(new_addrs.iter())
        .copied()
        .collect();

    let token_info = SimpleTokenCache::new();
    let fetch_config = build_fetch_config();
    let registry = Arc::new(PoolRegistry::new(CHAIN_ID));

    // ── Bootstrap ─────────────────────────────────────────────────────────
    let start_block = provider.get_block_number().await?;
    println!("[http] start_block = {start_block}");

    fetch_pools_into_registry(
        &provider,
        &initial_addrs,
        BlockNumberOrTag::Number(start_block),
        &token_info,
        &registry,
        &fetch_config,
    )
    .await?;
    println!("[http] bootstrapped {} pool(s)", registry.pool_count());

    // ── Start collector ────────────────────────────────────────────────────
    let mut handle = start_collector(
        Arc::clone(&provider),
        CollectorConfig {
            start_block,
            max_blocks_per_batch: 5,
            use_pending_blocks: false,
            use_websocket: false,
            websocket_urls: vec![],
            wait_time: 2_000, // poll every 2 s
        },
        Arc::clone(&registry),
        None,
        None,
    )
    .await?;

    println!("[http] collector running — sleeping 100 s …");
    tokio::time::sleep(Duration::from_secs(100)).await;

    // ── Dynamically add new pools ──────────────────────────────────────────
    println!("[http] adding {} new pool(s) …", new_addrs.len());
    handle
        .add_pools(new_addrs.clone(), &fetch_config, &token_info)
        .await?;
    println!(
        "[http] add_pools done (registry now has {} pool(s)) — sleeping 100 s …",
        registry.pool_count()
    );
    tokio::time::sleep(Duration::from_secs(100)).await;

    // ── Stop and verify ────────────────────────────────────────────────────
    handle.stop().await;
    let final_block = registry.get_last_processed_block();
    println!(
        "[http] stopped at block {final_block} — verifying {} pool(s) …",
        all_addrs.len()
    );

    assert_pools_match_chain_at(
        &provider,
        &registry,
        &fetch_config,
        &token_info,
        &all_addrs,
        final_block,
    )
    .await;

    println!("[http] ══ PASSED ══");
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 2 — WebSocket mode
// ---------------------------------------------------------------------------

/// End-to-end WebSocket mode test:
///
/// 1. Bootstrap the registry with 3 initial Ethereum mainnet pools.
/// 2. Start the collector in WebSocket mode (2 public WS endpoints).
/// 3. Wait 60 s for the WS bootstrap to complete and live events to flow.
/// 4. Call `handle.add_pools` with 2 new pool addresses.
/// 5. Immediately after `add_pools` returns, `last_processed_block` equals
///    the RPC-catchup end block (`final_chain_head`).  Verify the new pools
///    exactly at that block before any further WS events can be applied.
/// 6. Wait 60 s more.
/// 7. Stop the collector.
/// 8. Run a sanity check on all 5 pools: they must exist in the registry and
///    return non-zero output.
///
/// **Why no exact comparison after the second sleep?**
/// In WS mode `last_processed_block` is only advanced during the bootstrap
/// phase, not during live streaming.  After the second sleep the pool states
/// are several blocks ahead of any recorded block number, making an exact
/// block-aligned comparison impossible without per-pool block tracking.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_add_pools_ws() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();

    // Use HTTP for data fetching; WS for the live event stream
    let provider = Arc::new(ProviderBuilder::new().connect_http(HTTP_RPC.parse()?));

    // Initial pools (Base mainnet)
    let initial_addrs = vec![
        address!("0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640"),
        address!("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc"),
    ];
    // Pools to add dynamically
    let new_addrs = vec![
        address!("0xa43fe16908251ee70ef74718545e4fe6c5ccec9f"),
        address!("0x4e68ccd3e89f51c3074ca5072bbac773960dfa36"),
    ];
    let all_addrs: Vec<Address> = initial_addrs
        .iter()
        .chain(new_addrs.iter())
        .copied()
        .collect();

    let token_info = SimpleTokenCache::new();
    let fetch_config = build_fetch_config();
    let registry = Arc::new(PoolRegistry::new(CHAIN_ID));

    // ── Bootstrap ─────────────────────────────────────────────────────────
    let start_block = provider.get_block_number().await?;
    println!("[ws] start_block = {start_block}");

    fetch_pools_into_registry(
        &provider,
        &initial_addrs,
        BlockNumberOrTag::Number(start_block),
        &token_info,
        &registry,
        &fetch_config,
    )
    .await?;
    println!("[ws] bootstrapped {} pool(s)", registry.pool_count());

    // ── Start WS collector ─────────────────────────────────────────────────
    let mut handle = start_collector(
        Arc::clone(&provider),
        CollectorConfig {
            start_block,
            max_blocks_per_batch: 5,
            use_pending_blocks: false,
            use_websocket: true,
            websocket_urls: WS_RPCS.iter().map(|s| s.to_string()).collect(),
            wait_time: 0,
        },
        Arc::clone(&registry),
        None,
        None,
    )
    .await?;

    println!("[ws] WS collector running — sleeping 100 s …");
    tokio::time::sleep(Duration::from_secs(100)).await;

    // ── Dynamically add new pools ──────────────────────────────────────────
    println!("[ws] adding {} new pool(s) …", new_addrs.len());
    handle
        .add_pools(new_addrs.clone(), &fetch_config, &token_info)
        .await?;

    // Immediately after add_pools_ws:
    //   * last_processed_block = final_chain_head  (set by add_pools_ws step 7)
    //   * new pools are exactly at that block via RPC catchup
    //
    // We verify before the newly spawned WS task has had time to apply any
    // further events (Ethereum blocks are ~12 s apart, and this check takes
    // only a few seconds).
    let catchup_block = registry.get_last_processed_block();
    println!(
        "[ws] add_pools done (registry: {} pool(s)) — verifying {} new pool(s) at catchup block {catchup_block} …",
        registry.pool_count(),
        new_addrs.len()
    );
    assert_pools_match_chain_at(
        &provider,
        &registry,
        &fetch_config,
        &token_info,
        &new_addrs,
        catchup_block,
    )
    .await;
    println!("[ws] new-pool exact verification passed — sleeping 100 s …");

    tokio::time::sleep(Duration::from_secs(100)).await;

    // ── Stop and sanity-check ──────────────────────────────────────────────
    handle.stop().await;
    println!(
        "[ws] WS collector stopped — registry has {} pool(s)",
        registry.pool_count()
    );

    println!(
        "[ws] running sanity checks for all {} pool(s) …",
        all_addrs.len()
    );
    assert_pools_valid(&registry, &all_addrs).await;

    println!("[ws] ══ PASSED ══");
    Ok(())
}
