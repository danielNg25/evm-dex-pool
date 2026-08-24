//! The collector, run against the LB pool set **production would actually
//! discover** — GeckoTerminal's most-traded Avalanche pools — rather than
//! hand-picked fixtures.
//!
//! ```bash
//! # per-pool fetch cost, version detection vs GeckoTerminal (no soak)
//! cargo test --features collector test_lb_avalanche_config_fetch -- --ignored --nocapture
//!
//! # the full run: batch fetch, soak, convergence for all 18
//! cargo test --features collector test_lb_avalanche_config_collector -- --ignored --nocapture
//! ```
//!
//! # Why this exists alongside `lb_live_collector.rs`
//!
//! `lb_live_collector.rs` proves the collector converges on four fixtures
//! chosen *because* they were convenient: one reliably busy pair to drive the
//! anti-vacuity gate, one hooked pair, one v2.1, one dormant v2.0. Choosing
//! fixtures that way selects away from exactly the failure modes that matter
//! in production, where the pool set is not chosen at all — it arrives from a
//! discovery feed.
//!
//! The downstream bot (`arbitrage-bot-dashboard-be/src/discovery/`) builds its
//! pool set as: GeckoTerminal top pools by 24h volume → TVL filter →
//! `identify_pool_types` from this crate → fee-on-transfer screening. So the
//! honest test of the collector is that set: every LB pair in the top 100 by
//! volume, exotic decimals, unusual bin steps, whatever bin counts they happen
//! to carry, all fetched in one batch and all convergence-checked. Nothing is
//! excluded for being awkward.
//!
//! # Anti-vacuity
//!
//! Same discipline as `lb_live_collector.rs`: a soak that catches no activity
//! makes every convergence assertion below compare two identical states, which
//! proves nothing. The run is therefore gated on at least one pool's
//! `active_id` or `bins` having actually moved, and fails loudly otherwise
//! instead of reporting a green pass.

#![cfg(feature = "collector")]

// `common` also serves `lb_convergence.rs`; only part of it is used here.
#[allow(dead_code)]
mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{address, Address};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use anyhow::Result;

use evm_dex_pool::collector::{
    fetch_pools_into_registry, start_collector, CollectorConfig, PoolFetchConfig,
};
use evm_dex_pool::lb::{LBPool, LBVersion};
use evm_dex_pool::{PoolRegistry, TopicList};

use common::{assert_lb_pools_converge, CachingTokenInfo, CHAIN_ID, MULTICALL, RPC_URL};

// ---------------------------------------------------------------------------
// The pool set
// ---------------------------------------------------------------------------

/// One row of the GeckoTerminal listing, as returned by the API.
struct GeckoPool {
    address: Address,
    /// `relationships.dex.data.id`, verbatim. This is GeckoTerminal's own
    /// claim about which LB generation the pair belongs to, and the thing
    /// version detection is checked against.
    dex_slug: &'static str,
    /// `attributes.name`.
    pair: &'static str,
    /// `attributes.volume_usd.h24`, whole dollars.
    h24_volume_usd: u64,
    /// `attributes.reserve_in_usd`, whole dollars.
    tvl_usd: u64,
}

impl GeckoPool {
    /// GeckoTerminal's slug → the `LBVersion` this crate should detect.
    ///
    /// All three mappings are observed, not guessed: `traderjoe-v2-avalanche`
    /// was read back from the API for `0x1833…5A14`, the v2.0 USDC.e/USDC pair
    /// that `lb_version_detect.rs` pins as `LBVersion::V2_0`.
    fn expected_version(&self) -> LBVersion {
        match self.dex_slug {
            "traderjoe-v2-2-avalanche" => LBVersion::V2_2,
            "traderjoe-v2-1-avalanche" => LBVersion::V2_1,
            "traderjoe-v2-avalanche" => LBVersion::V2_0,
            other => panic!(
                "unmapped GeckoTerminal dex slug {other:?} — a new LB generation \
                 appeared in the listing and this mapping needs extending"
            ),
        }
    }

    fn label(&self) -> String {
        format!("{} {}", self.pair, self.dex_slug)
    }
}

/// Every Trader Joe Liquidity Book pool in GeckoTerminal's top 100 Avalanche
/// pools by 24h volume, ordered by that volume, descending.
///
/// # Provenance
///
/// ```text
/// GET https://api.geckoterminal.com/api/v2/networks/avax/pools
///     ?sort=h24_volume_usd_desc&page={1,2,3,4,5}
/// ```
///
/// 20 pools per page, so five pages are the top 100. Fetched **2026-08-25**,
/// then filtered to rows whose `relationships.dex.data.id` starts with
/// `traderjoe-v2`; 18 of the 100 qualified. `address`, `dex_slug`, `pair`,
/// `h24_volume_usd` and `tvl_usd` below are all copied from that response.
///
/// # Why pinned rather than fetched live
///
/// Fetching the list inside the test would make the fixture set change from
/// run to run, and this branch has already been bitten twice by time-varying
/// fixtures producing green runs that exercised nothing. A pinned list means a
/// failure is a real regression, not yesterday's volume ranking.
///
/// # This snapshot will age
///
/// The live listing drifts continuously: pools enter and leave the top 100 as
/// volume moves, TVL changes hourly, and a pair that is busy today can be
/// dormant next month. Expect the volume and TVL figures here to be stale
/// almost immediately — they are recorded for context, and nothing asserts on
/// them. What the test does depend on is that these addresses stay LB pairs of
/// the stated generation, which is immutable per address.
///
/// To refresh: re-run the query above, re-filter on the `traderjoe-v2` slug
/// prefix, and replace this table wholesale — including the fetch date.
const GECKO_LB_POOLS: [GeckoPool; 18] = [
    GeckoPool {
        address: address!("864d4e5ee7318e97483db7eb0912e09f161516ea"),
        dex_slug: "traderjoe-v2-2-avalanche",
        pair: "WAVAX/USDC",
        h24_volume_usd: 3_403_992,
        tvl_usd: 603_757,
    },
    GeckoPool {
        address: address!("4224f6f4c9280509724db2dbac314621e4465c29"),
        dex_slug: "traderjoe-v2-1-avalanche",
        pair: "BTC.b/USDC",
        h24_volume_usd: 192_195,
        tvl_usd: 667_717,
    },
    GeckoPool {
        address: address!("883ea72c2a46f7acb3820855344c43666c6cc5c0"),
        dex_slug: "traderjoe-v2-2-avalanche",
        pair: "sAVAX/WAVAX",
        h24_volume_usd: 95_000,
        tvl_usd: 70_668,
    },
    GeckoPool {
        address: address!("9b2cc8e6a2bbb56d6be4682891a91b0e48633c72"),
        dex_slug: "traderjoe-v2-1-avalanche",
        pair: "USDt/USDC",
        h24_volume_usd: 82_755,
        tvl_usd: 162_934,
    },
    GeckoPool {
        address: address!("2823299af89285ff1a1abf58db37ce57006fef5d"),
        dex_slug: "traderjoe-v2-2-avalanche",
        pair: "USDt/USDC",
        h24_volume_usd: 72_242,
        tvl_usd: 141_148,
    },
    GeckoPool {
        address: address!("8573f98175d816d520248b5facf40d309b1c9cee"),
        dex_slug: "traderjoe-v2-2-avalanche",
        pair: "AUSD/USDC",
        h24_volume_usd: 61_637,
        tvl_usd: 54_705,
    },
    GeckoPool {
        address: address!("55c211bbe9f63059a4a5a5e0c558c7e410412d98"),
        dex_slug: "traderjoe-v2-2-avalanche",
        pair: "SolvBTC/BTC.b",
        h24_volume_usd: 53_046,
        tvl_usd: 398_006,
    },
    GeckoPool {
        address: address!("99fd385c84e61c12a13975b198f38d87530f8554"),
        dex_slug: "traderjoe-v2-1-avalanche",
        pair: "USDt/DAI.e",
        h24_volume_usd: 46_562,
        tvl_usd: 21_967,
    },
    GeckoPool {
        address: address!("2f1da4bafd5f2508ec2e2e425036063a374993b6"),
        dex_slug: "traderjoe-v2-1-avalanche",
        pair: "DAI.e/USDC",
        h24_volume_usd: 43_919,
        tvl_usd: 30_631,
    },
    GeckoPool {
        address: address!("87eb2f90d7d0034571f343fb7429ae22c1bd9f72"),
        dex_slug: "traderjoe-v2-1-avalanche",
        pair: "WAVAX/USDt",
        h24_volume_usd: 42_805,
        tvl_usd: 113_877,
    },
    GeckoPool {
        address: address!("a78c3e56afb8d34a536825e2497255e78b388619"),
        dex_slug: "traderjoe-v2-2-avalanche",
        pair: "ARENA/WAVAX",
        h24_volume_usd: 42_671,
        tvl_usd: 148_784,
    },
    GeckoPool {
        address: address!("25d761e44ff595472a013e054dc15352413f093e"),
        dex_slug: "traderjoe-v2-2-avalanche",
        pair: "AUSD/USDt",
        h24_volume_usd: 35_248,
        tvl_usd: 102_187,
    },
    GeckoPool {
        address: address!("6fe050dc81b98e4464d3b4461a7995a8bf3350db"),
        dex_slug: "traderjoe-v2-1-avalanche",
        pair: "USDt/USDT.e",
        h24_volume_usd: 31_013,
        tvl_usd: 44_537,
    },
    GeckoPool {
        address: address!("e2b11d3002a2e49f1005e212e860f3b3ec73f985"),
        dex_slug: "traderjoe-v2-1-avalanche",
        pair: "USDT.e/USDC",
        h24_volume_usd: 30_923,
        tvl_usd: 47_173,
    },
    GeckoPool {
        address: address!("f0ef15733904131eb39790e64fa3c7575b41abfe"),
        dex_slug: "traderjoe-v2-2-avalanche",
        pair: "XAUt0/USDt",
        h24_volume_usd: 25_588,
        tvl_usd: 26_315,
    },
    GeckoPool {
        address: address!("cec377285abf370fdf872625d2742252656d631a"),
        dex_slug: "traderjoe-v2-2-avalanche",
        pair: "AUSD/USDt",
        h24_volume_usd: 22_801,
        tvl_usd: 52_479,
    },
    GeckoPool {
        address: address!("b3dc87bd570d8deaa960763234ef3d93cc789e6c"),
        dex_slug: "traderjoe-v2-2-avalanche",
        pair: "FOLKS/WAVAX",
        h24_volume_usd: 16_621,
        tvl_usd: 40_494,
    },
    GeckoPool {
        address: address!("856b38bf1e2e367f747dd4d3951dda8a35f1bf60"),
        dex_slug: "traderjoe-v2-2-avalanche",
        pair: "BTC.b/WAVAX",
        h24_volume_usd: 12_661,
        tvl_usd: 30_814,
    },
];

// ---------------------------------------------------------------------------
// Timing
// ---------------------------------------------------------------------------

/// Seconds to let the collector run before settling and stopping.
///
/// Matches `lb_live_collector.rs`. The reasoning there — Avalanche blocks
/// measured at ~1.08s, the busy WAVAX/USDC pair supplying most of the event
/// rate — applies unchanged, and this set contains that same pair plus 17
/// others, so the expected event count over a soak is strictly higher.
///
/// Override with `LB_AVAX_SOAK_SECS`.
const DEFAULT_SOAK_SECS: u64 = 180;

/// Seconds to keep the collector running after the soak, before stopping.
/// See `lb_live_collector.rs` for why a settle window exists at all.
///
/// Override with `LB_AVAX_SETTLE_SECS`.
const DEFAULT_SETTLE_SECS: u64 = 30;

fn secs_from_env(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Chunked, parallel — i.e. how a bootstrap actually runs, not a
/// rate-limit-friendly variant tuned to make this test comfortable.
fn build_fetch_config() -> PoolFetchConfig {
    PoolFetchConfig {
        chain_id: CHAIN_ID,
        multicall_address: Some(MULTICALL),
        factory_to_fee: HashMap::new(),
        aero_factory_addresses: vec![],
        chunk_size: 5,
        wait_time_between_chunks: 500,
        max_retries: 3,
        parallel_fetch: true,
    }
}

fn addresses() -> Vec<Address> {
    GECKO_LB_POOLS.iter().map(|p| p.address).collect()
}

/// Pull an `LBPool` out of the registry by address, cloned.
///
/// Returns `None` when the address is absent *or* holds a non-LB pool — the
/// caller reports that as a detection failure rather than panicking on a
/// failed downcast, so one misclassified pool does not hide the other 17.
async fn try_lb_pool(registry: &Arc<PoolRegistry>, addr: Address) -> Option<LBPool> {
    let pool_arc = registry.get_pool(&addr)?;
    let guard = pool_arc.read().await;
    guard.as_any().downcast_ref::<LBPool>().cloned()
}

async fn lb_pool(registry: &Arc<PoolRegistry>, addr: Address) -> LBPool {
    try_lb_pool(registry, addr)
        .await
        .unwrap_or_else(|| panic!("pool {addr} is missing from the registry or is not an LBPool"))
}

/// What moved on a pool between the snapshot and the end of the soak.
///
/// Chain-derived, event-driven fields only. `last_updated` is excluded on
/// purpose: `apply_log` stamps it from wall clock, so it moves whenever *any*
/// event is applied and would let the gate pass for reasons unrelated to this
/// pool's state.
fn describe_movement(before: &LBPool, after: &LBPool) -> Option<String> {
    let mut parts = Vec::new();
    if before.active_id != after.active_id {
        parts.push(format!(
            "active_id {} -> {}",
            before.active_id, after.active_id
        ));
    }
    if before.bins != after.bins {
        let changed = after
            .bins
            .iter()
            .filter(|(id, r)| before.bins.get(id) != Some(r))
            .count();
        let dropped = before
            .bins
            .keys()
            .filter(|id| !after.bins.contains_key(id))
            .count();
        parts.push(format!(
            "bins {} -> {} ({} added/changed, {} removed)",
            before.bins.len(),
            after.bins.len(),
            changed,
            dropped
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// Count LB logs each pool emitted over `(block_a, block_b]`.
///
/// Reporting only — the anti-vacuity *gate* is the state diff, because a log
/// count proves the chain moved, not that the collector saw it. This exists so
/// a failure can say how much traffic the soak covered, and so a divergence
/// between "logs existed" and "state changed" is visible.
async fn count_logs_per_pool<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    addrs: &[Address],
    block_a: u64,
    block_b: u64,
) -> Result<HashMap<Address, usize>> {
    let mut counts: HashMap<Address, usize> = addrs.iter().map(|&a| (a, 0)).collect();
    if block_b <= block_a {
        return Ok(counts);
    }
    // The endpoint rejects eth_getLogs ranges wider than 2048 blocks.
    const CHUNK: u64 = 2000;
    let mut from = block_a + 1;
    while from <= block_b {
        let to = (from + CHUNK - 1).min(block_b);
        let filter = Filter::new()
            .from_block(from)
            .to_block(to)
            .address(addrs.to_vec())
            .event_signature(LBPool::topics());
        for log in provider.get_logs(&filter).await? {
            *counts.entry(log.address()).or_insert(0) += 1;
        }
        from = to + 1;
    }
    Ok(counts)
}

/// Check every fetched pool's detected version against GeckoTerminal's slug.
///
/// Returns the mismatches rather than asserting, so the caller can print all
/// of them. Two independent sources disagreeing about a pool's generation is
/// a finding in its own right, and seeing only the first one would understate
/// it.
async fn version_mismatches(registry: &Arc<PoolRegistry>) -> Vec<String> {
    let mut out = Vec::new();
    for p in &GECKO_LB_POOLS {
        match try_lb_pool(registry, p.address).await {
            Some(pool) => {
                let expected = p.expected_version();
                if pool.version != expected {
                    out.push(format!(
                        "{} {}: GeckoTerminal says {} ({expected:?}), detection says {:?}",
                        p.address, p.pair, p.dex_slug, pool.version
                    ));
                }
            }
            None => out.push(format!(
                "{} {}: GeckoTerminal says {} but the pool is absent from the \
                 registry or was not classified as an LB pair",
                p.address, p.pair, p.dex_slug
            )),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Test 1: fetch cost and version detection
// ---------------------------------------------------------------------------

/// Fetch all 18 one at a time, timing each, and check every detected version
/// against GeckoTerminal's slug.
///
/// One address per `fetch_pools_into_registry` call is what makes per-pool
/// timing observable at all: the batched call the collector test uses fetches
/// a chunk concurrently, so its wall time attributes to no single pool. The
/// cost of that choice is that this is the *sequential* total, an upper bound
/// on bootstrap time; the batched figure is measured by
/// `test_lb_avalanche_config_collector`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_lb_avalanche_config_fetch() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .try_init();

    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let token_info = CachingTokenInfo::new();
    let config = build_fetch_config();
    let registry = Arc::new(PoolRegistry::new(CHAIN_ID));

    let block = provider.get_block_number().await?;
    println!("[fetch] block = {block}");

    let mut rows: Vec<(String, Duration)> = Vec::new();
    let started = Instant::now();

    for p in &GECKO_LB_POOLS {
        let t0 = Instant::now();
        // Deliberately not wrapped in a "keep going on failure" arm:
        // `fetch_pools_into_registry` is all-or-nothing by design, and the
        // point of running it on a discovery-shaped set is to find out whether
        // that holds. A `?` here reproduces exactly what a bootstrap would do.
        fetch_pools_into_registry(
            &provider,
            &[p.address],
            BlockNumberOrTag::Number(block),
            &token_info,
            &registry,
            &config,
        )
        .await?;
        let elapsed = t0.elapsed();

        let pool = lb_pool(&registry, p.address).await;
        let expected = p.expected_version();
        let verdict = if pool.version == expected {
            "ok"
        } else {
            "MISMATCH"
        };
        println!(
            "[fetch] {addr}  {pair:<14}  gecko={slug:<24}  detected={got:?}  {verdict:<8}  \
             bin_step={step:<4}  bins={bins:<5}  hooks={hooks:<5}  {ms} ms  \
             (pinned vol ${vol}, tvl ${tvl})",
            addr = p.address,
            pair = p.pair,
            slug = p.dex_slug,
            got = pool.version,
            step = pool.bin_step,
            bins = pool.bins.len(),
            hooks = pool.hooks_parameters.is_some(),
            ms = elapsed.as_millis(),
            vol = p.h24_volume_usd,
            tvl = p.tvl_usd,
        );
        rows.push((p.label(), elapsed));
    }

    let total = started.elapsed();
    rows.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
    println!(
        "[fetch] sequential total for {} pools: {:?}",
        rows.len(),
        total
    );
    println!("[fetch] slowest: {} at {:?}", rows[0].0, rows[0].1);
    println!(
        "[fetch] fastest: {} at {:?}",
        rows.last().unwrap().0,
        rows.last().unwrap().1
    );

    // `add_topics` extends without deduplicating, so 18 separate calls leave
    // 18 copies of LB's 8 topics behind. Printed rather than asserted on: it
    // is a property of the library, not something this test controls, and the
    // collector test below avoids it by fetching in one call.
    println!(
        "[fetch] registry topic count after {} separate fetch calls: {} (LB declares {})",
        GECKO_LB_POOLS.len(),
        registry.get_topics().len(),
        LBPool::topics().len(),
    );

    assert_eq!(
        registry.pool_count(),
        GECKO_LB_POOLS.len(),
        "[fetch] not every pool made it into the registry"
    );

    let mismatches = version_mismatches(&registry).await;
    for m in &mismatches {
        println!("[fetch] VERSION MISMATCH: {m}");
    }
    assert!(
        mismatches.is_empty(),
        "[fetch] {} of {} pools disagree with GeckoTerminal about their LB \
         generation:\n  {}",
        mismatches.len(),
        GECKO_LB_POOLS.len(),
        mismatches.join("\n  ")
    );
    println!(
        "[fetch] all {} detected versions match GeckoTerminal's dex slug",
        GECKO_LB_POOLS.len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 2: the collector over the whole discovered set
// ---------------------------------------------------------------------------

/// Bootstrap all 18 in one batch, run the real collector over them, then
/// require every one to equal a fresh fetch at the block the collector
/// reached.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_lb_avalanche_config_collector() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .try_init();

    let soak = secs_from_env("LB_AVAX_SOAK_SECS", DEFAULT_SOAK_SECS);
    let settle = secs_from_env("LB_AVAX_SETTLE_SECS", DEFAULT_SETTLE_SECS);

    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let token_info = CachingTokenInfo::new();
    let config = build_fetch_config();
    let all = addresses();

    // ── 1. Batch bootstrap at block A ───────────────────────────────────
    let live = Arc::new(PoolRegistry::new(CHAIN_ID));
    let block_a = provider.get_block_number().await?;
    println!("[collector] block_a = {block_a}");

    let t0 = Instant::now();
    fetch_pools_into_registry(
        &provider,
        &all,
        BlockNumberOrTag::Number(block_a),
        &token_info,
        &live,
        &config,
    )
    .await?;
    let batch_fetch = t0.elapsed();
    println!(
        "[collector] batched fetch of {} pools (chunk_size={}, parallel): {:?}",
        all.len(),
        config.chunk_size,
        batch_fetch
    );
    assert_eq!(
        live.pool_count(),
        all.len(),
        "[collector] not every pool made it into the registry"
    );

    // ── 2. Version detection vs GeckoTerminal ───────────────────────────
    let mismatches = version_mismatches(&live).await;
    for m in &mismatches {
        println!("[collector] VERSION MISMATCH: {m}");
    }
    assert!(
        mismatches.is_empty(),
        "[collector] {} of {} pools disagree with GeckoTerminal about their LB \
         generation:\n  {}",
        mismatches.len(),
        all.len(),
        mismatches.join("\n  ")
    );

    // ── 3. Snapshot, so the anti-vacuity gate has a baseline ────────────
    let mut snapshots: Vec<(String, Address, LBPool)> = Vec::new();
    for p in &GECKO_LB_POOLS {
        let pool = lb_pool(&live, p.address).await;
        println!(
            "[collector] snapshot {:<14} {} version={:?} bin_step={} bins={}",
            p.pair,
            p.address,
            pool.version,
            pool.bin_step,
            pool.bins.len()
        );
        snapshots.push((p.pair.to_string(), p.address, pool));
    }

    // ── 4. Start the real collector (polling mode) ──────────────────────
    let mut handle = start_collector(
        Arc::clone(&provider),
        CollectorConfig {
            start_block: block_a,
            max_blocks_per_batch: 5,
            use_pending_blocks: false,
            use_websocket: false,
            websocket_urls: vec![],
            wait_time: 2_000,
            refetch_algebra_fee: false,
        },
        Arc::clone(&live),
        None,
        None,
        None,
    )
    .await?;

    // ── 5. Soak, settle, stop ───────────────────────────────────────────
    println!("[collector] running — soaking {soak}s ...");
    tokio::time::sleep(Duration::from_secs(soak)).await;
    println!(
        "[collector] soak done, last_processed_block = {}",
        live.get_last_processed_block()
    );
    println!("[collector] settling {settle}s before stop ...");
    tokio::time::sleep(Duration::from_secs(settle)).await;
    handle.stop().await;

    let block_b = live.get_last_processed_block();
    println!(
        "[collector] block_b = {block_b} ({} blocks)",
        block_b - block_a
    );
    assert!(
        block_b > block_a,
        "[collector] the collector never advanced past block_a ({block_a}) — \
         it processed nothing at all"
    );

    // ── 6. How much traffic the window actually carried ─────────────────
    let counts = count_logs_per_pool(&provider, &all, block_a, block_b).await?;
    let total_logs: usize = counts.values().sum();
    for p in &GECKO_LB_POOLS {
        println!(
            "[collector] on-chain logs in ({block_a}, {block_b}] for {:<14} {}: {}",
            p.pair,
            p.address,
            counts.get(&p.address).copied().unwrap_or(0)
        );
    }
    println!("[collector] total logs over the soak window: {total_logs}");

    // ── 7. Anti-vacuity gate ────────────────────────────────────────────
    let mut moved = Vec::new();
    let mut post: Vec<(String, Address, LBPool)> = Vec::new();
    for (pair, addr, before) in &snapshots {
        let after = lb_pool(&live, *addr).await;
        match describe_movement(before, &after) {
            Some(what) => {
                println!("[collector] {pair} {addr} MOVED: {what}");
                moved.push(format!("{pair} {addr}"));
            }
            None => println!("[collector] {pair} {addr} unchanged over the soak"),
        }
        post.push((pair.clone(), *addr, after));
    }
    assert!(
        !moved.is_empty(),
        "[collector] the soak caught no activity: not one of the {} pools changed \
         its active_id or bins over blocks {block_a}..={block_b} ({total_logs} LB \
         logs on chain in that window). Every convergence assertion below would \
         be comparing two identical states and proving nothing. The top pool in \
         this set traded $3.4M in 24h when the list was pinned, so a silent \
         window is itself the finding — investigate before raising \
         LB_AVAX_SOAK_SECS.",
        all.len()
    );
    println!(
        "[collector] {}/{} pools moved: {:?}",
        moved.len(),
        all.len(),
        moved
    );

    // ── 8. Fresh, independent fetch at block B ──────────────────────────
    let fresh = Arc::new(PoolRegistry::new(CHAIN_ID));
    let t1 = Instant::now();
    fetch_pools_into_registry(
        &provider,
        &all,
        BlockNumberOrTag::Number(block_b),
        &token_info,
        &fresh,
        &config,
    )
    .await?;
    println!("[collector] batched refetch at block_b: {:?}", t1.elapsed());

    // ── 9. Collector-built state must equal the fresh fetch, for all 18 ─
    for (pair, addr, live_pool) in &post {
        let fresh_pool = lb_pool(&fresh, *addr).await;
        assert_lb_pools_converge(live_pool, &fresh_pool, &format!("{pair} {addr}"));
        println!(
            "[collector] {pair} {addr} converged across {} bins",
            fresh_pool.bins.len()
        );
    }

    println!(
        "[collector] PASSED — {} pools, blocks {block_a}..={block_b}, {total_logs} logs, \
         {} moved",
        all.len(),
        moved.len()
    );
    Ok(())
}
