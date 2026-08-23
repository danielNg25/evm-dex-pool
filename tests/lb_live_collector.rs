//! Live-collector convergence: state the **real collector pipeline** builds
//! must equal state fetched fresh at the block the collector reached.
//!
//! ```bash
//! cargo test --features collector test_lb_live_collector -- --ignored --nocapture
//! ```
//!
//! # Why this exists alongside `lb_convergence.rs`
//!
//! `lb_convergence.rs` proves `apply_log` is correct, but it replays logs by
//! calling `apply_log` directly — the collector is never involved. Everything
//! between the chain and `apply_log` is therefore untested by it: the
//! `BlockSource` batching and cursor arithmetic, the `enrich_log_timestamps`
//! call sites, `EventProcessor`'s grouping, and the WebSocket drain path. A
//! defect in any of those — a missed enrichment call, an off-by-one on a
//! batch boundary, a dropped or double-applied batch — passes every existing
//! test and only surfaces in production.
//!
//! `lb_collector.rs` does run a live collector, but its only state assertion
//! is that pools quote non-zero, which a pool carrying silently wrong bin
//! reserves satisfies just as well as a correct one.
//!
//! This test closes that gap: run the real collector, then compare the
//! registry against an independent fresh fetch at the same block.
//!
//! # Anti-vacuity
//!
//! Comparing state at block A against state at block B proves nothing if no
//! events landed in between — both sides are then trivially identical. So the
//! run is gated on the pool state having *actually moved*: at least one pool's
//! `active_id` or `bins` map must differ from the snapshot taken right after
//! the initial fetch. A quiet soak window fails loudly instead of passing
//! green.

#![cfg(feature = "collector")]

// Only `assert_lb_pools_converge` and `CachingTokenInfo` are used here; the
// rest of the shared module serves `lb_convergence.rs`. The allow keeps that
// from spraying dead-code warnings over this binary without touching the
// shared file.
#[allow(dead_code)]
mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{address, Address};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use anyhow::Result;

use evm_dex_pool::collector::{
    fetch_pools_into_registry, start_collector, CollectorConfig, PoolFetchConfig,
};
use evm_dex_pool::lb::LBPool;
use evm_dex_pool::{PoolRegistry, TopicList};

use common::{assert_lb_pools_converge, CachingTokenInfo, CHAIN_ID, MULTICALL, RPC_URL};

const WS_URL: &str = "wss://api.avax.network/ext/bc/C/ws";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// v2.2 WAVAX/USDC. The activity driver: measured at 123 logs per 2000 blocks
/// (~1 log every 17s) on 2026-08-23, an order of magnitude busier than any
/// other LB pair checked. Without it the anti-vacuity gate below would need a
/// ~7 minute soak to be reliable; with it, ~2 minutes suffices.
const V22_BUSY: Address = address!("0x864d4e5ee7318e97483db7eb0912e09f161516ea");

/// v2.2 WAVAX/USDC binStep=20, **with hooks installed** — the only fixture in
/// the suite whose `hooks_parameters` is `Some(non-zero)`. Measured at ~7 logs
/// per 2000 blocks, so it usually contributes nothing to a short soak; it is
/// here to prove the collector handles a hooked pair, not to drive the gate.
const V22_HOOKS: Address = address!("0x8573f98175d816d520248b5facf40d309b1c9cee");

/// v2.1 WAVAX/USDT binStep=20. Measured at ~29 logs per 2000 blocks.
const V21: Address = address!("0x4224f6f4c9280509724db2dbac314621e4465c29");

/// v2.0 USDC.e/USDC. Registered so the collector has to cope with a
/// mixed-generation registry (v2.0 event topics are distinct from v2.1+, and
/// registering this pool is what puts them in the registry's topic filter),
/// but **deliberately excluded from the convergence assertions**: it is
/// dormant — 199 Swaps in 500,000 blocks, and zero logs in the most recent
/// 2000 — so a soak of this length will essentially never move it. Asserting
/// convergence on a pool that provably did not change is the vacuous pass
/// this file exists to prevent, so it is checked only for "still registered,
/// still quotable, did not break the collector".
const V20_DORMANT: Address = address!("0x18332988456C4Bd9ABa6698ec748b331516F5A14");

/// The pools whose state the convergence assertion covers.
const CONVERGENCE_POOLS: [(&str, Address); 3] = [
    ("v2.2-busy", V22_BUSY),
    ("v2.2-hooks", V22_HOOKS),
    ("v2.1", V21),
];

// ---------------------------------------------------------------------------
// Timing
// ---------------------------------------------------------------------------

/// Seconds to let the collector run before settling and stopping.
///
/// Default deliberately exceeds the 90s originally scoped for this test. The
/// arithmetic behind 90s assumed 2s Avalanche blocks; measured on 2026-08-23,
/// 2000 blocks span 2153s, i.e. ~1.08s per block, so 2000 blocks is ~36
/// minutes, not ~67. At the measured combined rate for the three convergence
/// fixtures (159 logs / 2153s ≈ 0.074/s) a 90s soak expects ~6.6 events —
/// which sounds fine until you note that the busy pool contributes 77% of it
/// and the Poisson probability of a completely silent 90s window is still
/// ~0.1%. 180s halves the flake exposure again and costs a minute.
///
/// Override with `LB_SOAK_SECS` when re-pinning or debugging.
const DEFAULT_SOAK_SECS: u64 = 180;

/// Seconds to keep the collector running after the soak, before stopping.
///
/// This is the mitigation for a known WebSocket-mode race: there,
/// `last_processed_block` is the maximum block among *drained* events, so
/// events belonging to that same block could still be in flight when we stop,
/// and the fresh fetch at `block_b` would then legitimately disagree with the
/// registry. Letting the collector run on for a while after the soak means
/// further events arrive and push `block_b` forward, leaving the block we
/// finally compare at safely behind the head with all of its events drained.
///
/// Override with `LB_SETTLE_SECS`.
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

/// Pull an `LBPool` out of the registry by address, cloned.
///
/// Cloning rather than borrowing keeps the registry lock held for the minimum
/// time and lets the caller hold snapshots across `.await` points.
async fn lb_pool_from_registry(registry: &Arc<PoolRegistry>, addr: Address) -> LBPool {
    let pool_arc = registry
        .get_pool(&addr)
        .unwrap_or_else(|| panic!("pool {addr} not in registry"));
    let guard = pool_arc.read().await;
    guard
        .as_any()
        .downcast_ref::<LBPool>()
        .unwrap_or_else(|| panic!("pool {addr} is registered but is not an LBPool"))
        .clone()
}

/// What moved on a pool between the snapshot and the end of the soak.
///
/// Only chain-derived, event-driven fields count. `last_updated` is excluded
/// on purpose: `apply_log` stamps it from wall clock, so it changes whenever
/// *any* event is applied and would make this gate pass for reasons unrelated
/// to the pool's actual state.
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

/// Count LB logs each pool actually emitted over `(block_a, block_b]`.
///
/// Reporting only — the anti-vacuity *gate* is the state diff above, because a
/// log count proves the chain moved, not that the collector saw it. This is
/// here so a failure report can say how much traffic the soak covered, and so
/// a divergence between "logs existed" and "state changed" is visible.
async fn count_logs_per_pool<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    addresses: &[Address],
    block_a: u64,
    block_b: u64,
) -> Result<HashMap<Address, usize>> {
    let mut counts: HashMap<Address, usize> = addresses.iter().map(|&a| (a, 0)).collect();
    if block_b <= block_a {
        return Ok(counts);
    }
    // The endpoint rejects eth_getLogs ranges wider than 2048 blocks. A soak
    // this short never gets close, but a long LB_SOAK_SECS could.
    const CHUNK: u64 = 2000;
    let mut from = block_a + 1;
    while from <= block_b {
        let to = (from + CHUNK - 1).min(block_b);
        let filter = Filter::new()
            .from_block(from)
            .to_block(to)
            .address(addresses.to_vec())
            .event_signature(LBPool::topics());
        for log in provider.get_logs(&filter).await? {
            *counts.entry(log.address()).or_insert(0) += 1;
        }
        from = to + 1;
    }
    Ok(counts)
}

// ---------------------------------------------------------------------------
// The shared body
// ---------------------------------------------------------------------------

async fn live_collector_converges(use_websocket: bool, label: &str) -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();

    let soak = secs_from_env("LB_SOAK_SECS", DEFAULT_SOAK_SECS);
    let settle = secs_from_env("LB_SETTLE_SECS", DEFAULT_SETTLE_SECS);

    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let token_info = CachingTokenInfo::new();
    let fetch_config = build_fetch_config();

    let all_pools: Vec<Address> = CONVERGENCE_POOLS
        .iter()
        .map(|(_, a)| *a)
        .chain(std::iter::once(V20_DORMANT))
        .collect();

    // ── 1. Fetch pools at block A ───────────────────────────────────────
    let live = Arc::new(PoolRegistry::new(CHAIN_ID));
    let block_a = provider.get_block_number().await?;
    println!("[{label}] block_a = {block_a}");

    fetch_pools_into_registry(
        &provider,
        &all_pools,
        BlockNumberOrTag::Number(block_a),
        &token_info,
        &live,
        &fetch_config,
    )
    .await?;
    assert_eq!(
        live.pool_count(),
        all_pools.len(),
        "[{label}] not every fixture made it into the registry"
    );

    // ── 2. Snapshot, so step 7 has something to compare against ─────────
    let mut snapshots: Vec<(&str, Address, LBPool)> = Vec::new();
    for (name, addr) in CONVERGENCE_POOLS {
        let pool = lb_pool_from_registry(&live, addr).await;
        println!(
            "[{label}] snapshot {name} {addr}: version={:?} active_id={} bins={}",
            pool.version,
            pool.active_id,
            pool.bins.len()
        );
        snapshots.push((name, addr, pool));
    }

    // ── 3. Start the real collector ─────────────────────────────────────
    let mut handle = start_collector(
        Arc::clone(&provider),
        CollectorConfig {
            start_block: block_a,
            max_blocks_per_batch: 5,
            use_pending_blocks: false,
            use_websocket,
            websocket_urls: if use_websocket {
                vec![WS_URL.to_string()]
            } else {
                vec![]
            },
            wait_time: 2_000,
            refetch_algebra_fee: false,
        },
        Arc::clone(&live),
        None,
        None,
        None,
    )
    .await?;

    // ── 4. Soak ─────────────────────────────────────────────────────────
    println!("[{label}] collector running — soaking {soak}s ...");
    tokio::time::sleep(Duration::from_secs(soak)).await;
    println!(
        "[{label}] soak done, last_processed_block = {}",
        live.get_last_processed_block()
    );

    // ── 5. Settle, then stop. `stop` awaits the updater's join handle, so
    //       `last_processed_block` is committed once it returns. ─────────
    println!("[{label}] settling {settle}s before stop ...");
    tokio::time::sleep(Duration::from_secs(settle)).await;
    handle.stop().await;

    // ── 6. Read the block the collector actually reached ────────────────
    let block_b = live.get_last_processed_block();
    println!(
        "[{label}] block_b = {block_b} ({} blocks)",
        block_b - block_a
    );
    assert!(
        block_b > block_a,
        "[{label}] collector never advanced past block_a ({block_a}) — \
         it processed nothing at all"
    );

    let counts = count_logs_per_pool(&provider, &all_pools, block_a, block_b).await?;
    let total_logs: usize = counts.values().sum();
    for (name, addr) in CONVERGENCE_POOLS {
        println!(
            "[{label}] on-chain logs in ({block_a}, {block_b}] for {name}: {}",
            counts.get(&addr).copied().unwrap_or(0)
        );
    }
    println!(
        "[{label}] on-chain logs for v2.0-dormant (excluded from convergence): {}",
        counts.get(&V20_DORMANT).copied().unwrap_or(0)
    );
    println!("[{label}] total logs over the soak window: {total_logs}");

    // ── 7. Anti-vacuity gate: the state must actually have moved ────────
    let mut moved = Vec::new();
    let mut post: Vec<(&str, LBPool)> = Vec::new();
    for (name, addr, before) in &snapshots {
        let after = lb_pool_from_registry(&live, *addr).await;
        match describe_movement(before, &after) {
            Some(what) => {
                println!("[{label}] {name} MOVED: {what}");
                moved.push(*name);
            }
            None => println!("[{label}] {name} unchanged over the soak"),
        }
        post.push((name, after));
    }
    assert!(
        !moved.is_empty(),
        "[{label}] the soak window caught no activity: not one of {:?} changed \
         its active_id or bins over blocks {block_a}..={block_b} ({total_logs} \
         LB logs on chain in that window). The convergence assertions below \
         would be comparing two identical states and proving nothing. Raise \
         LB_SOAK_SECS or re-pin the fixtures onto busier pairs.",
        CONVERGENCE_POOLS.map(|(n, _)| n)
    );

    // ── 8. Fresh, independent fetch at block B ──────────────────────────
    let fresh = Arc::new(PoolRegistry::new(CHAIN_ID));
    fetch_pools_into_registry(
        &provider,
        &all_pools,
        BlockNumberOrTag::Number(block_b),
        &token_info,
        &fresh,
        &fetch_config,
    )
    .await?;

    // ── 9. Collector-built state must equal the fresh fetch ─────────────
    for (name, live_pool) in &post {
        let addr = CONVERGENCE_POOLS
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, a)| *a)
            .unwrap();
        let fresh_pool = lb_pool_from_registry(&fresh, addr).await;
        assert_lb_pools_converge(live_pool, &fresh_pool, &format!("{label}/{name}"));
        println!(
            "[{label}] {name} converged across {} bins",
            fresh_pool.bins.len()
        );
    }

    // The v2.0 pool is not convergence-checked (see V20_DORMANT). It is only
    // required to have survived: still registered, and still an LB pool the
    // collector could have applied events to.
    let v20 = lb_pool_from_registry(&live, V20_DORMANT).await;
    println!(
        "[{label}] v2.0-dormant survived: version={:?} active_id={} bins={}",
        v20.version,
        v20.active_id,
        v20.bins.len()
    );
    assert!(
        !v20.bins.is_empty(),
        "[{label}] v2.0 pool lost all its bins while the collector ran"
    );

    println!(
        "[{label}] PASSED — blocks {block_a}..={block_b}, {total_logs} logs, moved: {moved:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// LatestBlock polling mode (`use_websocket: false`), i.e. `LatestBlockSource`
/// driving the batches over plain HTTP RPC.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_lb_live_collector_polling() -> Result<()> {
    live_collector_converges(false, "polling").await
}

/// WebSocket mode (`use_websocket: true`), i.e. `WebsocketListener` feeding
/// the `EventQueue` that `WebsocketBlockSource` drains.
///
/// Verified live before this test was written: `wss://api.avax.network/ext/bc/C/ws`
/// accepts the connection and streams `eth_subscribe` log notifications for
/// these fixtures (12 logs observed in a 90s probe). Note the endpoint serves
/// *only* subscriptions on that path — `eth_blockNumber` over it returns
/// "method does not exist" — which is fine, because the collector uses the WS
/// provider solely for `subscribe_logs` and drives every other RPC call
/// through the HTTP provider passed to `start_collector`.
///
/// Also note the logs arrive with `blockTimestamp: null`, so this path really
/// does depend on `enrich_log_timestamps` being wired into
/// `WebsocketBlockSource`; without it `time_of_last_update` would diverge and
/// the convergence assertion would fail.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_lb_live_collector_websocket() -> Result<()> {
    live_collector_converges(true, "websocket").await
}
