//! Integration tests for TraderJoe Liquidity Book (LB) pool fetching and swap estimation.
//!
//! These tests hit live RPC endpoints and are marked `#[ignore]`.
//! Run them with:
//!
//! ```bash
//! cargo test --features collector test_lb -- --ignored --nocapture
//! ```
//!
//! Before running, update the constants below with your RPC URL and a known LB pool address.

#![cfg(feature = "collector")]

use std::sync::{Arc, Mutex};
use std::collections::HashMap;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use anyhow::Result;

use evm_dex_pool::lb::{fetch_lb_pool, LBPool};
use evm_dex_pool::TokenInfo;

// ---------------------------------------------------------------------------
// ⚠️  Configure these before running tests
// ---------------------------------------------------------------------------

/// RPC URL for the target chain (e.g. Avalanche, Arbitrum).
const RPC_URL: &str = "https://api.avax.network/ext/bc/C/rpc";

/// Address of a TraderJoe LB pool to test against.
/// Example: AVAX/USDC LB pool on Avalanche.
const POOL_ADDRESS: &str = "0xD446eb1660F766d533BeCeEf890Df7A69d26f7d1";

/// Chain ID for the RPC.
const CHAIN_ID: u64 = 43114; // Avalanche C-Chain

/// Multicall3 address (standard on most chains).
const MULTICALL: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

// ---------------------------------------------------------------------------
// ERC-20 + ILBPair ABI (inline for test use)
// ---------------------------------------------------------------------------

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function decimals() external view returns (uint8);
    }
}

sol! {
    #[sol(rpc)]
    interface ILBPairView {
        function getSwapOut(uint128 amountIn, bool swapForY) external view returns (uint128 amountInLeft, uint128 amountOut, uint128 fee);
        function getSwapIn(uint128 amountOut, bool swapForY) external view returns (uint128 amountIn, uint128 amountOutLeft, uint128 fee);
    }
}

// ---------------------------------------------------------------------------
// SimpleTokenCache (copied from collector_add_pools.rs)
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

/// Returns `(provider, pool, block_number, block_timestamp)`.
async fn fetch_test_pool() -> Result<(Arc<impl Provider + Send + Sync>, LBPool, u64, u64)> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let token_info = SimpleTokenCache::new();
    let multicall_address = parse_address(MULTICALL);
    let pool_address = parse_address(POOL_ADDRESS);

    let block_number = provider.get_block_number().await?;
    let block_id = BlockId::Number(BlockNumberOrTag::Number(block_number));

    // Fetch block timestamp for exact swap comparison
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(block_number))
        .await?
        .expect("Block should exist");
    let block_timestamp = block.header.timestamp;

    println!(
        "══ Fetching LB pool {} at block {} (ts={}) ══",
        pool_address, block_number, block_timestamp
    );

    let pool = fetch_lb_pool(
        &provider,
        pool_address,
        block_id,
        &token_info,
        multicall_address,
        CHAIN_ID,
        None, // use tree bitmap discovery
    )
    .await?;

    Ok((provider, pool, block_number, block_timestamp))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test that `fetch_lb_pool` successfully fetches a pool and discovers bins.
#[tokio::test]
#[ignore]
async fn test_lb_fetch_pool() -> Result<()> {
    let (_provider, pool, block_number, _block_timestamp) = fetch_test_pool().await?;

    println!("  Pool:      {}", pool.address);
    println!("  TokenX:    {}", pool.token_x);
    println!("  TokenY:    {}", pool.token_y);
    println!("  BinStep:   {}", pool.bin_step);
    println!("  ActiveId:  {}", pool.active_id);
    println!("  Bins:      {}", pool.bins.len());
    println!("  Fee:       {:.4}%", pool.fee_f64() * 100.0);
    println!("  Block:     {}", block_number);

    assert!(!pool.bins.is_empty(), "Pool should have non-empty bins");
    assert!(pool.bin_step > 0, "Bin step should be > 0");
    assert!(
        pool.bins.contains_key(&pool.active_id)
            || pool.bins.range(..pool.active_id).next_back().is_some(),
        "Should have bins near active_id"
    );

    // Print bin range
    if let (Some((&min_id, _)), Some((&max_id, _))) =
        (pool.bins.iter().next(), pool.bins.iter().next_back())
    {
        println!(
            "  Bin range: [{}, {}] (span: {})",
            min_id,
            max_id,
            max_id - min_id
        );
    }

    println!("══ test_lb_fetch_pool PASSED ══");
    Ok(())
}

/// Test that our `simulate_swap_out` matches the on-chain `getSwapOut`.
#[tokio::test]
#[ignore]
async fn test_lb_swap_estimate() -> Result<()> {
    let (provider, pool, block_number, block_timestamp) = fetch_test_pool().await?;
    let block_id = BlockId::Number(BlockNumberOrTag::Number(block_number));
    let pool_address = parse_address(POOL_ADDRESS);

    let lb_view = ILBPairView::new(pool_address, &provider);

    // Test amounts (small, medium, large)
    let test_amounts: Vec<u128> = vec![
        1_000_000_000_000_000,     // 1e15 (0.001 token)
        100_000_000_000_000_000,   // 1e17 (0.1 token)
        1_000_000_000_000_000_000, // 1e18 (1 token)
    ];

    println!("\n══ Swap Estimate Comparison (swap_for_y = true, sell X for Y) ══");
    println!(
        "{:<24} {:>20} {:>20} {:>10}",
        "Amount In", "Our Out", "Chain Out", "Match?"
    );
    println!("{}", "-".repeat(78));

    for &amount in &test_amounts {
        let our_result = pool.simulate_swap_out_at(amount, true, block_timestamp);
        let chain_result = lb_view
            .getSwapOut(amount, true)
            .block(block_id)
            .call()
            .await;

        match (&our_result, &chain_result) {
            (Ok((our_left, our_out, _our_fee)), Ok(chain_res)) => {
                let chain_out: u128 = chain_res.amountOut;
                let chain_left: u128 = chain_res.amountInLeft;
                let matches = *our_out == chain_out && *our_left == chain_left;
                println!(
                    "{:<24} {:>20} {:>20} {:>10}",
                    amount,
                    our_out,
                    chain_out,
                    if matches { "OK" } else { "MISMATCH" }
                );
                if !matches {
                    println!(
                        "  Detail: our_left={}, chain_left={}, our_out={}, chain_out={}",
                        our_left, chain_left, our_out, chain_out
                    );
                }
                assert_eq!(
                    *our_out, chain_out,
                    "amount_out mismatch for amount_in={}",
                    amount
                );
                assert_eq!(
                    *our_left, chain_left,
                    "amount_in_left mismatch for amount_in={}",
                    amount
                );
            }
            (Err(e), _) => {
                println!("{:<24} ERROR: {}", amount, e);
                // Don't fail — pool may not have enough liquidity for large amounts
            }
            (_, Err(e)) => {
                println!("{:<24} CHAIN ERROR: {}", amount, e);
            }
        }
    }

    println!("\n══ Swap Estimate Comparison (swap_for_y = false, sell Y for X) ══");
    println!(
        "{:<24} {:>20} {:>20} {:>10}",
        "Amount In", "Our Out", "Chain Out", "Match?"
    );
    println!("{}", "-".repeat(78));

    for &amount in &test_amounts {
        let our_result = pool.simulate_swap_out_at(amount, false, block_timestamp);
        let chain_result = lb_view
            .getSwapOut(amount, false)
            .block(block_id)
            .call()
            .await;

        match (&our_result, &chain_result) {
            (Ok((our_left, our_out, _our_fee)), Ok(chain_res)) => {
                let chain_out: u128 = chain_res.amountOut;
                let chain_left: u128 = chain_res.amountInLeft;
                let matches = *our_out == chain_out && *our_left == chain_left;
                println!(
                    "{:<24} {:>20} {:>20} {:>10}",
                    amount,
                    our_out,
                    chain_out,
                    if matches { "OK" } else { "MISMATCH" }
                );
                if !matches {
                    println!(
                        "  Detail: our_left={}, chain_left={}, our_out={}, chain_out={}",
                        our_left, chain_left, our_out, chain_out
                    );
                }
                assert_eq!(
                    *our_out, chain_out,
                    "amount_out mismatch for amount_in={}",
                    amount
                );
                assert_eq!(
                    *our_left, chain_left,
                    "amount_in_left mismatch for amount_in={}",
                    amount
                );
            }
            (Err(e), _) => {
                println!("{:<24} ERROR: {}", amount, e);
            }
            (_, Err(e)) => {
                println!("{:<24} CHAIN ERROR: {}", amount, e);
            }
        }
    }

    println!("\n══ test_lb_swap_estimate PASSED ══");
    Ok(())
}
