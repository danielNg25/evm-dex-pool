//! End-to-end collector lifecycle test for TraderJoe LB pools.
//!
//! Runs a live collector on Avalanche with a mix of LFJ v2.1 and v2.2 pools,
//! exercises `add_pools` and `remove_pools`, and verifies registry state.
//!
//! ```bash
//! cargo test --features collector test_lb_collector -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{address, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use anyhow::Result;

use evm_dex_pool::collector::{
    fetch_pools_into_registry, start_collector, CollectorConfig, PoolFetchConfig,
};
use evm_dex_pool::{PoolRegistry, TokenInfo};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const RPC_URL: &str = "https://api.avax.network/ext/bc/C/rpc";
const CHAIN_ID: u64 = 43114;
const MULTICALL: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

/// How long to wait per phase (seconds).
const PHASE_SLEEP: u64 = 100;

// v2.2 pools (tree at slot 7)
const V22_POOL_1: Address = address!("0x8573f98175d816d520248b5facf40d309b1c9cee"); // WAVAX/USDC binStep=20
const V22_POOL_2: Address = address!("0xcec377285abf370fdf872625d2742252656d631a"); // WAVAX/USDC binStep=10

// v2.1 pools (tree at slot 8)
const V21_POOL_1: Address = address!("0x4224f6f4c9280509724db2dbac314621e4465c29"); // WAVAX/USDT binStep=20
const V21_POOL_2: Address = address!("0x9b2cc8e6a2bbb56d6be4682891a91b0e48633c72"); // BTC/USDC binStep=10

// ---------------------------------------------------------------------------
// ABI
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
// Helpers
// ---------------------------------------------------------------------------

fn parse_address(s: &str) -> Address {
    s.parse::<Address>().expect("Invalid address")
}

fn build_fetch_config() -> PoolFetchConfig {
    PoolFetchConfig {
        chain_id: CHAIN_ID,
        multicall_address: Some(parse_address(MULTICALL)),
        factory_to_fee: HashMap::new(),
        aero_factory_addresses: vec![],
        chunk_size: 5,
        wait_time_between_chunks: 500,
        max_retries: 3,
        parallel_fetch: true,
        lb_bin_depth: None,
    }
}

/// Sanity check: each address exists in registry and produces non-zero output.
/// Tries progressively smaller amounts to handle pools with different token decimals.
async fn assert_pools_valid(registry: &Arc<PoolRegistry>, addresses: &[Address]) {
    let test_amounts = [
        U256::from(1_000_000_000_000_000u128), // 1e15 (0.001 for 18-dec tokens)
        U256::from(1_000_000_000u128),          // 1e9  (10 for 8-dec tokens like BTC)
        U256::from(1_000_000u128),              // 1e6  (1 for 6-dec tokens like USDC)
        U256::from(1_000u128),                  // 1e3  (tiny fallback)
    ];

    for &addr in addresses {
        let pool_arc = registry
            .get_pool(&addr)
            .unwrap_or_else(|| panic!("Pool {} not found in registry", addr));
        let pool = pool_arc.read().await;
        let (token0, _) = pool.tokens();

        let mut out = U256::ZERO;
        let mut used_amount = U256::ZERO;
        for &amount in &test_amounts {
            if let Ok(result) = pool.calculate_output(&token0, amount) {
                if result > U256::ZERO {
                    out = result;
                    used_amount = amount;
                    break;
                }
            }
        }

        println!("    pool={} amount_in={} output={}", addr, used_amount, out);
        assert!(
            out > U256::ZERO,
            "Pool {} returned zero output for all test amounts — state may be corrupted",
            addr
        );
    }
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_lb_collector_lifecycle() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();

    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let token_info = SimpleTokenCache::new();
    let fetch_config = build_fetch_config();
    let registry = Arc::new(PoolRegistry::new(CHAIN_ID));

    // ── Phase 1: Bootstrap & Start ──────────────────────────────────────
    println!("\n══ Phase 1: Bootstrap (v2.2 + v2.1 pools) ══");

    let initial_addrs = vec![V22_POOL_1, V21_POOL_1];
    let start_block = provider.get_block_number().await?;
    println!("  start_block = {}", start_block);

    fetch_pools_into_registry(
        &provider,
        &initial_addrs,
        BlockNumberOrTag::Number(start_block),
        &token_info,
        &registry,
        &fetch_config,
    )
    .await?;
    println!(
        "  Bootstrapped {} pool(s) into registry",
        registry.pool_count()
    );
    assert_eq!(registry.pool_count(), 2);

    let mut handle = start_collector(
        Arc::clone(&provider),
        CollectorConfig {
            start_block,
            max_blocks_per_batch: 5,
            use_pending_blocks: false,
            use_websocket: false,
            websocket_urls: vec![],
            wait_time: 2_000,
        },
        Arc::clone(&registry),
        None,
        None,
    )
    .await?;

    println!("  Collector running — sleeping {}s ...", PHASE_SLEEP);
    tokio::time::sleep(Duration::from_secs(PHASE_SLEEP)).await;
    println!(
        "  last_processed_block = {}",
        registry.get_last_processed_block()
    );

    // ── Phase 2: Remove a pool ──────────────────────────────────────────
    println!("\n══ Phase 2: Remove pool (v2.2 WAVAX/USDC) ══");

    let removed = handle.remove_pools(&[V22_POOL_1]);
    println!("  remove_pools returned: {}", removed);
    assert_eq!(removed, 1, "Expected to remove 1 pool");
    assert!(
        registry.get_pool(&V22_POOL_1).is_none(),
        "V22_POOL_1 should be removed"
    );
    assert!(
        registry.get_pool(&V21_POOL_1).is_some(),
        "V21_POOL_1 should still be present"
    );
    assert_eq!(registry.pool_count(), 1);
    println!("  Registry: {} pool(s)", registry.pool_count());

    println!("  Sleeping {}s ...", PHASE_SLEEP);
    tokio::time::sleep(Duration::from_secs(PHASE_SLEEP)).await;
    println!(
        "  last_processed_block = {}",
        registry.get_last_processed_block()
    );

    // ── Phase 3: Add new pools ──────────────────────────────────────────
    println!("\n══ Phase 3: Add pools (v2.2 + v2.1) ══");

    let new_addrs = vec![V22_POOL_2, V21_POOL_2];
    println!("  Adding {} pool(s): {:?}", new_addrs.len(), new_addrs);
    handle
        .add_pools(new_addrs.clone(), &fetch_config, &token_info)
        .await?;
    println!(
        "  add_pools done — registry now has {} pool(s)",
        registry.pool_count()
    );
    assert_eq!(registry.pool_count(), 3);
    assert!(registry.get_pool(&V21_POOL_1).is_some());
    assert!(registry.get_pool(&V22_POOL_2).is_some());
    assert!(registry.get_pool(&V21_POOL_2).is_some());

    println!("  Sleeping {}s ...", PHASE_SLEEP);
    tokio::time::sleep(Duration::from_secs(PHASE_SLEEP)).await;

    // ── Phase 4: Stop & Verify ──────────────────────────────────────────
    println!("\n══ Phase 4: Stop & Verify ══");

    handle.stop().await;
    let final_block = registry.get_last_processed_block();
    println!(
        "  Stopped at block {} — verifying {} pool(s)",
        final_block,
        registry.pool_count()
    );

    let remaining_addrs = vec![V21_POOL_1, V22_POOL_2, V21_POOL_2];
    assert_pools_valid(&registry, &remaining_addrs).await;

    // Confirm removed pool is still gone
    assert!(
        registry.get_pool(&V22_POOL_1).is_none(),
        "V22_POOL_1 should still be removed after stop"
    );

    println!("\n══ test_lb_collector_lifecycle PASSED ══");
    println!("  Blocks processed: {} → {}", start_block, final_block);
    Ok(())
}
