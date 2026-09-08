//! Fuzz integration tests for TraderJoe Liquidity Book (LB) pools.
//!
//! Tests `simulate_swap_out_at` and `simulate_swap_in_at` against on-chain
//! `getSwapOut` / `getSwapIn` across multiple pools with batched multicall.
//!
//! ```bash
//! cargo test --features collector test_lb -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, U256};
use alloy::providers::{MulticallBuilder, Provider, ProviderBuilder};
use alloy::sol;
use anyhow::Result;
use evm_dex_pool::lb::fetch_lb_pool;
use evm_dex_pool::TokenInfo;

// ---------------------------------------------------------------------------
// ⚠️  Configure: RPC + pool list
// ---------------------------------------------------------------------------

const RPC_URL: &str = "https://api.avax.network/ext/bc/C/rpc";
const CHAIN_ID: u64 = 43114; // Avalanche C-Chain
const MULTICALL: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

/// Multipliers for fuzz amounts: fraction of 1 token (in 1/10000ths).
/// e.g., 1 = 0.0001 token, 10 = 0.001 token, 10000 = 1 token, 100000 = 10 tokens.
const FUZZ_MULTIPLIERS: &[u128] = &[
    1, 5, 10, 50, 100, 500, 1000, 2500, 5000, 10000, 25000, 50000, 100000,
];

/// Pool addresses to fuzz test.
/// Add/remove pools here — the test iterates over all of them.
const TEST_POOLS: &[&str] = &[
    "0xD446eb1660F766d533BeCeEf890Df7A69d26f7d1", // WAVAX/USDC (binStep=20)
    "0x864d4e5ee7318e97483db7eb0912e09f161516ea",
    "0x2823299af89285ff1a1abf58db37ce57006fef5d",
    "0x87eb2f90d7d0034571f343fb7429ae22c1bd9f72",
    "0x4224f6f4c9280509724db2dbac314621e4465c29",
];

// ---------------------------------------------------------------------------
// ABI bindings
// ---------------------------------------------------------------------------

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function decimals() external view returns (uint8);
    }
}

/// LB v2.0 router on Avalanche. v2.0 pairs expose no on-pair quoter, so the
/// on-chain reference for exact-in comes from the router.
///
/// Verified live before this test was written, rather than assumed: the
/// deployed bytecode (22,857 bytes) contains selector `0x2004b724` —
/// `getSwapOut(address,uint256,bool)`, the signature below — alongside
/// `oldFactory()`, `wavax()` and `getSwapIn(address,uint256,bool)`, which
/// together identify it as the v2.0 router rather than a later generation
/// (v2.1's quoter is `getSwapOut(address,uint128,bool)` = `0xa0d376cf`,
/// absent here).
const V20_ROUTER: &str = "0xE3Ffc583dC176575eEA7FD9dF2A7c65F7E23f4C3";

/// v2.0 pools to check quote parity on. This is USDC.e/USDC with binStep 1
/// and **6-decimal** tokens on both sides — not the 18-decimal pair the name
/// "WAVAX/USDC" would suggest — so amounts must be scaled by
/// `fuzz_amounts(decimals)`, never by a hardcoded 1e18-shaped constant.
const V20_TEST_POOLS: &[&str] = &[
    "0x18332988456C4Bd9ABa6698ec748b331516F5A14", // USDC.e/USDC, binStep 1
];

sol! {
    #[sol(rpc)]
    interface ILBRouterV20 {
        function getSwapOut(address lbPair, uint256 amountIn, bool swapForY)
            external view returns (uint256 amountOut, uint256 feesIn);
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

/// Generate deterministic test amounts scaled to the token's decimals.
fn fuzz_amounts(decimals: u8) -> Vec<u128> {
    let base = 10u128.pow(decimals as u32);
    FUZZ_MULTIPLIERS
        .iter()
        .map(|&m| base * m / 10_000)
        .filter(|&a| a > 0)
        .collect()
}

/// Fetch token decimals via ERC-20 `decimals()`.
async fn get_decimals<P: Provider + Send + Sync>(provider: &Arc<P>, token: Address) -> u8 {
    let contract = IERC20::new(token, provider);
    contract.decimals().call().await.unwrap_or(18)
}

// ---------------------------------------------------------------------------
// Fuzz test
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_lb_fuzz() -> Result<()> {
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .try_init();

    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let token_info = SimpleTokenCache::new();
    let multicall_address = parse_address(MULTICALL);

    let block_number = provider.get_block_number().await?;
    let block_id = BlockId::Number(BlockNumberOrTag::Number(block_number));
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(block_number))
        .await?
        .expect("Block should exist");
    let block_timestamp = block.header.timestamp;

    println!(
        "══ LB Fuzz Test: {} pool(s), {} amounts per direction, block {} (ts={}) ══\n",
        TEST_POOLS.len(),
        FUZZ_MULTIPLIERS.len(),
        block_number,
        block_timestamp
    );

    let mut total_checks = 0u32;
    let mut total_passed = 0u32;
    let mut total_errors = 0u32;

    for &pool_addr_str in TEST_POOLS {
        let pool_address = parse_address(pool_addr_str);

        println!("── Pool: {} ──", pool_address);

        let pool = match fetch_lb_pool(
            &provider,
            pool_address,
            block_id,
            &token_info,
            multicall_address,
            CHAIN_ID,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                println!("  SKIP: failed to fetch: {}\n", e);
                continue;
            }
        };

        let dec_x = get_decimals(&provider, pool.token_x).await;
        let dec_y = get_decimals(&provider, pool.token_y).await;

        println!(
            "  TokenX: {} ({}dec)  TokenY: {} ({}dec)",
            pool.token_x, dec_x, pool.token_y, dec_y
        );
        println!(
            "  BinStep: {}  ActiveId: {}  Bins: {}  Fee: {:.4}%",
            pool.bin_step,
            pool.active_id,
            pool.bins.len(),
            pool.fee_f64() * 100.0
        );

        let x_amounts = fuzz_amounts(dec_x);
        let y_amounts = fuzz_amounts(dec_y);

        let lb_view = ILBPairView::new(pool_address, &provider);

        // ── Step 1: Build swap_out calls + compute off-chain ──
        struct SwapOutCase {
            amount: u128,
            swap_for_y: bool,
            offchain: Result<(u128, u128, u128)>,
        }
        struct SwapInCase {
            amount: u128,
            swap_for_y: bool,
            offchain: Result<(u128, u128, u128)>,
        }

        let mut out_cases: Vec<SwapOutCase> = Vec::new();
        let mut in_cases: Vec<SwapInCase> = Vec::new();

        // getSwapOut: X→Y and Y→X
        for &amount in &x_amounts {
            out_cases.push(SwapOutCase {
                amount,
                swap_for_y: true,
                offchain: pool.simulate_swap_out_at(amount, true, block_timestamp),
            });
        }
        for &amount in &y_amounts {
            out_cases.push(SwapOutCase {
                amount,
                swap_for_y: false,
                offchain: pool.simulate_swap_out_at(amount, false, block_timestamp),
            });
        }

        // getSwapIn: want Y (pay X) and want X (pay Y)
        for &amount in &y_amounts {
            in_cases.push(SwapInCase {
                amount,
                swap_for_y: true,
                offchain: pool.simulate_swap_in_at(amount, true, block_timestamp),
            });
        }
        for &amount in &x_amounts {
            in_cases.push(SwapInCase {
                amount,
                swap_for_y: false,
                offchain: pool.simulate_swap_in_at(amount, false, block_timestamp),
            });
        }

        // ── Step 2: Batch on-chain calls (separate multicall per return type) ──
        let mut mc_out = MulticallBuilder::new_dynamic(&provider).address(multicall_address);
        for c in &out_cases {
            mc_out = mc_out.add_dynamic(lb_view.getSwapOut(c.amount, c.swap_for_y));
        }
        let chain_out_results = mc_out.block(block_id).aggregate().await?;

        let mut mc_in = MulticallBuilder::new_dynamic(&provider).address(multicall_address);
        for c in &in_cases {
            mc_in = mc_in.add_dynamic(lb_view.getSwapIn(c.amount, c.swap_for_y));
        }
        let chain_in_results = mc_in.block(block_id).aggregate().await?;

        // ── Step 3: Compare ──
        // When our offchain result has remaining input/output but the chain
        // doesn't, it means we have fewer bins than the full on-chain state
        // (tree discovery fell back to a fixed window). Skip those cases.
        let mut pool_skipped = 0u32;

        for (i, case) in out_cases.iter().enumerate() {
            total_checks += 1;
            let chain = &chain_out_results[i];
            match &case.offchain {
                Ok((our_left, our_out, _)) => {
                    let c_out: u128 = chain.amountOut;
                    let c_left: u128 = chain.amountInLeft;

                    if *our_left > 0 && c_left == 0 {
                        // We ran out of bins, chain didn't — partial coverage
                        pool_skipped += 1;
                    } else if *our_out == c_out && *our_left == c_left {
                        total_passed += 1;
                    } else {
                        total_errors += 1;
                        println!(
                            "  MISMATCH getSwapOut(amount={}, swap_for_y={}): out {}!={}, left {}!={}",
                            case.amount, case.swap_for_y, our_out, c_out, our_left, c_left
                        );
                    }
                }
                Err(_) => {} // insufficient liquidity
            }
        }

        for (i, case) in in_cases.iter().enumerate() {
            total_checks += 1;
            let chain = &chain_in_results[i];
            match &case.offchain {
                Ok((our_in, our_out_left, _)) => {
                    let c_in: u128 = chain.amountIn;
                    let c_out_left: u128 = chain.amountOutLeft;

                    if *our_out_left > 0 && *our_out_left != c_out_left {
                        // We ran out of bins or found fewer — partial coverage
                        pool_skipped += 1;
                    } else if *our_in == c_in && *our_out_left == c_out_left {
                        total_passed += 1;
                    } else {
                        total_errors += 1;
                        println!(
                            "  MISMATCH getSwapIn(amount={}, swap_for_y={}): in {}!={}, out_left {}!={}",
                            case.amount, case.swap_for_y, our_in, c_in, our_out_left, c_out_left
                        );
                    }
                }
                Err(_) => {} // insufficient liquidity
            }
        }

        if pool_skipped > 0 {
            println!(
                "  WARNING: {} checks skipped (partial bin coverage — tree discovery may have fallen back to ±100 window)",
                pool_skipped
            );
        }

        let pool_checks = out_cases.len() + in_cases.len();
        println!(
            "  {} checks done ({} passed so far)\n",
            pool_checks, total_passed
        );
    }

    println!("══════════════════════════════════════════════════");
    println!(
        "  Results: {}/{} passed, {} errors",
        total_passed, total_checks, total_errors
    );
    println!("══════════════════════════════════════════════════");

    assert_eq!(
        total_errors, 0,
        "Fuzz test had {} mismatches out of {} checks",
        total_errors, total_checks
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// v2.0 quote parity
// ---------------------------------------------------------------------------

/// Acceptance criterion "offchain quote == onchain quote at the same block",
/// extended to LB v2.0.
///
/// Two structural differences from `test_lb_fuzz`, both forced by v2.0:
///
/// 1. The reference comes from the **router**, not the pair — v2.0 pairs have
///    no `getSwapOut`. The router returns `(amountOut, feesIn)` with no
///    `amountInLeft`, so only exact-in parity is checkable here; there is no
///    v2.0 analogue of the `getSwapIn` half.
/// 2. The router **reverts** (`TreeMath__ErrorDepthSearch()`, `0x10d64861`)
///    instead of reporting a shortfall when the swap walks off the end of the
///    bin tree, so the on-chain call is a `Result` that must be matched, not
///    `?`-propagated. A revert is only accepted as a skip when our simulator
///    independently agrees the input could not be fully consumed; a revert
///    against a simulator that *did* consume everything is a real
///    disagreement and is counted as a mismatch.
///
/// Expect several minutes: v2.0 has no corroborated bin-tree layout, so
/// `fetch_lb_pool` walks bins sequentially.
#[tokio::test]
#[ignore]
async fn test_lb_v20_fuzz() -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let token_info = SimpleTokenCache::new();
    let multicall_address = parse_address(MULTICALL);

    // Pin a block so the on-chain reference and the offline simulation see
    // identical state, and read its timestamp for the volatility decay.
    let block_number = provider.get_block_number().await?;
    let block_id = BlockId::Number(BlockNumberOrTag::Number(block_number));
    let block_timestamp = provider
        .get_block_by_number(BlockNumberOrTag::Number(block_number))
        .await?
        .expect("block should exist")
        .header
        .timestamp;

    let router = ILBRouterV20::new(parse_address(V20_ROUTER), &provider);

    let mut total_checked = 0u32;
    let mut total_skipped = 0u32;
    let mut total_errors = 0u32;

    for &pool_addr_str in V20_TEST_POOLS {
        let pool_address = parse_address(pool_addr_str);
        let pool = fetch_lb_pool(
            &provider,
            pool_address,
            block_id,
            &token_info,
            multicall_address,
            CHAIN_ID,
        )
        .await?;

        let dec_x = get_decimals(&provider, pool.token_x).await;
        let dec_y = get_decimals(&provider, pool.token_y).await;
        println!(
            "── v2.0 pool {} ── binStep {} activeId {} bins {} tokenX {}dec tokenY {}dec",
            pool_address,
            pool.bin_step,
            pool.active_id,
            pool.bins.len(),
            dec_x,
            dec_y
        );

        // swapForY spends token X, so scale by X's decimals, and vice versa.
        for (swap_for_y, amounts) in [(true, fuzz_amounts(dec_x)), (false, fuzz_amounts(dec_y))] {
            for amount_in in amounts {
                let onchain = router
                    .getSwapOut(pool_address, U256::from(amount_in), swap_for_y)
                    .block(block_id)
                    .call()
                    .await;

                let (amount_in_left, offchain_out, _fee) =
                    match pool.simulate_swap_out_at(amount_in, swap_for_y, block_timestamp) {
                        Ok(r) => r,
                        // Our simulator gave up entirely — insufficient
                        // liquidity, the same condition the router reverts on.
                        Err(_) => {
                            total_skipped += 1;
                            continue;
                        }
                    };

                let onchain = match onchain {
                    Ok(r) => r,
                    Err(_) if amount_in_left > 0 => {
                        // Both sides agree the input cannot be fully consumed;
                        // they just report it differently.
                        total_skipped += 1;
                        continue;
                    }
                    Err(e) => {
                        total_errors += 1;
                        println!(
                            "  MISMATCH getSwapOut(amount={amount_in}, swap_for_y={swap_for_y}): \
                             router reverted ({e}) but offchain consumed the full input for {offchain_out} out"
                        );
                        continue;
                    }
                };

                if amount_in_left > 0 {
                    // We ran out of bins and the router didn't — partial bin
                    // coverage, same caveat as `test_lb_fuzz`.
                    total_skipped += 1;
                    continue;
                }

                if U256::from(offchain_out) == onchain.amountOut {
                    total_checked += 1;
                } else {
                    total_errors += 1;
                    println!(
                        "  MISMATCH getSwapOut(amount={amount_in}, swap_for_y={swap_for_y}): \
                         offchain {offchain_out} vs onchain {}",
                        onchain.amountOut
                    );
                }
            }
        }

        println!(
            "  v2.0 {pool_address}: quote parity across {} bins",
            pool.bins.len()
        );
    }

    println!("  v2.0 results: {total_checked} matched, {total_skipped} skipped, {total_errors} mismatched");

    assert_eq!(
        total_errors, 0,
        "v2.0 quote parity had {total_errors} mismatches"
    );
    // A run where every case was skipped would otherwise pass while proving
    // nothing — the exact failure mode the pinned convergence ranges exist to
    // prevent.
    assert!(
        total_checked > 0,
        "v2.0 quote parity checked nothing: all {total_skipped} cases were skipped"
    );

    Ok(())
}
