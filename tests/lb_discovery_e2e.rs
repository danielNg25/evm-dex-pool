//! End-to-end proof that the LB **discovery** path works against live
//! Avalanche: find pairs from `LBPairCreated` factory logs, triage what came
//! back, fetch a bounded sample, run the real collector over it, and require
//! the collector-built state to equal a fresh fetch at the block it reached.
//!
//! ```bash
//! cargo test --features collector --test lb_discovery_e2e -- --ignored --nocapture
//! ```
//!
//! # Why this exists alongside `lb_live_collector.rs`
//!
//! `lb_live_collector.rs` runs the same collector, but over four pools that a
//! human picked because they were known to be busy, well-formed, and one of
//! each generation. That proves the pipeline works on curated input. It says
//! nothing about what happens when the pool list is whatever a factory
//! actually emitted — which is what a production consumer watching
//! `POOL_CREATED_TOPICS` will get.
//!
//! This file removes the curation from the *pool* selection: every address it
//! touches is decoded out of a real `LBPairCreated` log through the crate's
//! own `ILBFactory` binding, and the sample it soaks is chosen by a mechanical
//! rule over measured on-chain activity, not by hand.
//!
//! # What is still pinned, and why that is not curation-by-the-back-door
//!
//! The *block windows* are pinned (see [`WINDOWS`]). They have to be: LB pair
//! creation on Avalanche has stopped — a scan of the 100,000 blocks below the
//! head on 2026-08-24 returned **zero** `LBPairCreated` events from all three
//! factories — so a head-derived window discovers nothing at all. The windows
//! are pinned per *deployment era* (one per factory generation, plus one dense
//! burst), and every pair inside them is taken, including the dead ones. No
//! individual pair is named anywhere in this file.
//!
//! # Anti-vacuity
//!
//! Two gates, both of which fail loudly rather than passing green:
//!
//! 1. **Pre-soak.** If triage finds no discovered pair with any recent
//!    on-chain activity, the soak provably cannot move anything, and the test
//!    aborts *before* burning four minutes to compare two identical states.
//! 2. **Post-soak.** At least one sampled pool's `active_id` or `bins` must
//!    have changed while the collector ran, exactly as in
//!    `lb_live_collector.rs`.

#![cfg(feature = "collector")]

// Only part of the shared module is used here; the rest serves
// `lb_convergence.rs`. Same allow as `lb_live_collector.rs`.
#[allow(dead_code)]
mod common;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Result;

use evm_dex_pool::collector::{
    fetch_pools_into_registry, start_collector, CollectorConfig, PoolFetchConfig,
};
use evm_dex_pool::lb::{
    detect_lb_version, get_lb_factories_by_chain_id, get_lb_factory_addresses_by_chain_id,
    get_lb_version_by_factory, LBPool, LBVersion,
};
use evm_dex_pool::{PoolRegistry, TopicList, POOL_CREATED_TOPICS};

use common::{assert_lb_pools_converge, CachingTokenInfo, CHAIN_ID, MULTICALL, RPC_URL};

// Local `#[sol(rpc)]` bindings. `evm_dex_pool::contracts{,_rpc}` are
// `pub(crate)`, so integration tests re-derive what they need from the same
// ABI JSON the library compiles — the pattern `lb_convergence.rs` and
// `lb_discovery.rs` already use. Deriving rather than hardcoding is the point:
// a field-order or type mistake in the ABI shows up as a decode failure here.
alloy::sol! {
    #[sol(rpc)]
    ILBFactory,
    "contracts/ABI/ILBFactory.json"
}

alloy::sol! {
    #[sol(rpc)]
    ILBPair,
    "contracts/ABI/ILBPair.json"
}

alloy::sol! {
    #[sol(rpc)]
    ILBPairV20,
    "contracts/ABI/ILBPairV20.json"
}

// ---------------------------------------------------------------------------
// Discovery windows
// ---------------------------------------------------------------------------

/// Pinned block windows to scan for `LBPairCreated`, one per LB deployment
/// era plus one dense creation burst.
///
/// Chosen so that all three factory generations in
/// [`get_lb_factories_by_chain_id`] actually emit something — a single window
/// only ever covers one generation, because each factory's pairs were created
/// in its own era. Every window is scanned against *all three* factory
/// addresses regardless; which one answers is discovery's output, not this
/// file's input.
///
/// Measured 2026-08-24 against `https://api.avax.network/ext/bc/C/rpc`:
///
/// | window        | events | factory that answered |
/// |---------------|--------|-----------------------|
/// | `v2.0-era`    | 6      | v2.0 only             |
/// | `v2.1-era`    | 6      | v2.1 only             |
/// | `v2.2-era-a`  | 3      | v2.2 only             |
/// | `v2.2-era-b`  | 3      | v2.2 only             |
/// | `launchpad`   | 148    | v2.2 only             |
///
/// `launchpad` is the window from `tests/lb_discovery.rs`, widened to 6,000
/// blocks. It is a single actor minting memecoins in a loop — see the module
/// docs of [`triage_all`] for what that does to the triage numbers. It is
/// deliberately included: a consumer scanning factory logs in production will
/// hit exactly this, and it is where the interesting degenerate cases live.
const WINDOWS: [(&str, u64, u64); 5] = [
    ("v2.0-era", 22_440_424, 22_446_424),
    ("v2.1-era", 28_386_780, 28_392_780),
    ("v2.2-era-a", 49_155_734, 49_161_734),
    ("v2.2-era-b", 54_211_023, 54_217_023),
    ("launchpad", 91_406_504, 91_412_504),
];

/// Floor on total unique pairs discovered. Not a target — a canary. If the
/// endpoint's log retention ever moves a window out of reach, `eth_getLogs`
/// returns an empty array rather than an error, and every assertion after
/// discovery would then pass over an empty set. Measured total is 166.
const MIN_EXPECTED_PAIRS: usize = 120;

/// The endpoint rejects `eth_getLogs` ranges wider than 2048 blocks.
const GETLOGS_CHUNK: u64 = 2000;

/// How far back the liveness probe looks. Same width as
/// `common::REPLAY_BLOCKS` and for the same reason: one `eth_getLogs` call per
/// address batch.
const LIVENESS_BLOCKS: u64 = 2000;

/// Upper bound on pairs handed to the collector.
///
/// Bounded on purpose. Discovery yields 166 pairs and LB fetches are not
/// cheap: measured 2026-08-24, one v2.2 pair with 2,788 bins takes 9.4s, and
/// v2.0 pairs have no corroborated bin-tree layout so they walk bins
/// sequentially — 28.3s for 103 bins. Fetching all 166 twice (once to seed the
/// registry, once for the fresh comparison) would dominate the run without
/// testing anything the sample does not.
const SAMPLE_SIZE: usize = 15;

/// How many empty (zero-reserve) pairs to force into the sample.
///
/// The activity ranking would never select one — an empty pool emits nothing —
/// yet "created but never funded" is the single most common thing discovery
/// yields, so the run must show the pipeline swallowing it: fetched with zero
/// bins, registered, collected over, and converged.
const EMPTY_SAMPLE_SLOTS: usize = 2;

/// Bounded fan-out for the triage probes. One multicall per pair per round.
const PROBE_CONCURRENCY: usize = 16;

// ---------------------------------------------------------------------------
// Timing
// ---------------------------------------------------------------------------

/// Seconds to run the collector. Same reasoning as `lb_live_collector.rs`:
/// Avalanche blocks measured at ~1.08s, so 180s is ~165 blocks, and the
/// busiest pair discovery turns up runs ~97 logs per 2,000 blocks — about 8
/// expected events. Override with `LB_SOAK_SECS`.
const DEFAULT_SOAK_SECS: u64 = 180;

/// Seconds to keep collecting after the soak so the block finally compared at
/// is safely behind the head. Override with `LB_SETTLE_SECS`.
const DEFAULT_SETTLE_SECS: u64 = 30;

fn secs_from_env(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One pair as it came out of a `LBPairCreated` log — nothing here is from an
/// RPC state call, it is all decoded from the event plus the emitter address.
#[derive(Debug, Clone)]
struct Discovered {
    address: Address,
    factory: Address,
    /// Generation resolved from the emitting factory via
    /// [`get_lb_version_by_factory`], with no RPC round trip. Cross-checked
    /// against [`detect_lb_version`] during triage.
    factory_version: Option<LBVersion>,
    token_x: Address,
    token_y: Address,
    bin_step: U256,
    block: u64,
    window: &'static str,
}

/// What a cheap, public-getter-only probe says about a discovered pair.
#[derive(Debug, Clone)]
enum Triage {
    /// [`detect_lb_version`] found no LB discriminator. Would be a serious
    /// finding: the address came out of an LB factory's own creation event.
    NotLbPair,
    /// An LB pair whose reserves are zero. Verified equivalent to "the bin
    /// tree is empty, so `fetch_lb_pool` will return zero bins": checked on
    /// 2026-08-24 against a discovered v2.2 pair whose `getReserves()` is
    /// `(0,0)`, whose tree root storage word is zero, and which fetches with
    /// `bins.len() == 0`.
    Empty { version: LBVersion },
    /// An LB pair holding reserves. Expected to fetch with at least one bin.
    Viable {
        version: LBVersion,
        active_id: u32,
        bin_step: u16,
        reserve_x: u128,
        reserve_y: u128,
    },
    /// The probe itself failed. Kept as a distinct outcome rather than being
    /// folded into `NotLbPair`, so "not an LB pair" and "could not tell" stay
    /// different answers.
    ProbeFailed {
        version: Option<LBVersion>,
        why: String,
    },
}

impl Triage {
    fn version(&self) -> Option<LBVersion> {
        match self {
            Triage::NotLbPair => None,
            Triage::Empty { version } => Some(*version),
            Triage::Viable { version, .. } => Some(*version),
            Triage::ProbeFailed { version, .. } => *version,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Triage::NotLbPair => "not-an-lb-pair",
            Triage::Empty { .. } => "empty",
            Triage::Viable { .. } => "viable",
            Triage::ProbeFailed { .. } => "probe-failed",
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Discover
// ---------------------------------------------------------------------------

/// Scan every pinned window against every LB factory this crate knows for
/// Avalanche, decoding each hit through the `ILBFactory` binding.
async fn discover<P: Provider + Send + Sync>(provider: &Arc<P>) -> Result<Vec<Discovered>> {
    let factories = get_lb_factory_addresses_by_chain_id(CHAIN_ID);
    assert!(
        !factories.is_empty(),
        "no LB factories registered for chain {CHAIN_ID}"
    );
    println!(
        "[discover] {} factory address(es) for chain {CHAIN_ID}:",
        factories.len()
    );
    for f in get_lb_factories_by_chain_id(CHAIN_ID) {
        println!(
            "[discover]   {} {:?} verified={}",
            f.address, f.version, f.verified
        );
    }

    // The topic is not hardcoded and not re-derived independently: it is the
    // one the library publishes for consumers to watch. If production ever
    // dropped it from POOL_CREATED_TOPICS, discovery in the wild would silently
    // stop finding LB pairs, and this test would keep passing off its own
    // binding. Tie the two together.
    let topic = ILBFactory::LBPairCreated::SIGNATURE_HASH;
    assert!(
        POOL_CREATED_TOPICS.contains(&topic),
        "LBPairCreated is not in POOL_CREATED_TOPICS, so a consumer watching \
         the library's own creation-topic list would never see LB pairs"
    );

    let mut by_address: BTreeMap<Address, Discovered> = BTreeMap::new();
    let mut per_window: Vec<(&str, usize)> = Vec::new();

    for (name, from_block, to_block) in WINDOWS {
        let mut window_hits = 0usize;
        let mut from = from_block;
        while from <= to_block {
            let to = (from + GETLOGS_CHUNK - 1).min(to_block);
            let filter = Filter::new()
                .address(factories.clone())
                .event_signature(topic)
                .from_block(from)
                .to_block(to);
            for log in provider.get_logs(&filter).await? {
                let factory = log.address();
                let block = log
                    .block_number
                    .expect("a mined log always carries a block number");
                let decoded = log.log_decode::<ILBFactory::LBPairCreated>()?.inner.data;
                window_hits += 1;
                by_address
                    .entry(decoded.LBPair)
                    .or_insert_with(|| Discovered {
                        address: decoded.LBPair,
                        factory,
                        factory_version: get_lb_version_by_factory(CHAIN_ID, &factory.to_string()),
                        token_x: decoded.tokenX,
                        token_y: decoded.tokenY,
                        bin_step: decoded.binStep,
                        block,
                        window: name,
                    });
            }
            from = to + 1;
        }
        println!("[discover] window {name} {from_block}..={to_block}: {window_hits} event(s)");
        per_window.push((name, window_hits));
    }

    let pairs: Vec<Discovered> = by_address.into_values().collect();

    println!("[discover] {} unique pair(s) total", pairs.len());
    let mut by_factory: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_bin_step: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_window: BTreeMap<&str, usize> = BTreeMap::new();
    for p in &pairs {
        *by_factory
            .entry(format!("{} ({:?})", p.factory, p.factory_version))
            .or_default() += 1;
        *by_bin_step.entry(p.bin_step.to_string()).or_default() += 1;
        *by_window.entry(p.window).or_default() += 1;
    }
    println!("[discover] by factory:");
    for (k, v) in &by_factory {
        println!("[discover]   {k}: {v}");
    }
    println!("[discover] by bin step:");
    for (k, v) in &by_bin_step {
        println!("[discover]   binStep={k}: {v}");
    }
    println!("[discover] by window:");
    for (k, v) in &by_window {
        println!("[discover]   {k}: {v}");
    }

    assert!(
        pairs.len() >= MIN_EXPECTED_PAIRS,
        "discovery returned {} pairs, below the {MIN_EXPECTED_PAIRS} floor — \
         a pinned window has probably fallen out of the endpoint's log \
         retention (eth_getLogs answers an unreachable range with an empty \
         array, not an error). Re-pin WINDOWS.",
        pairs.len()
    );

    // Every hit must come from an address this crate lists as an LB factory:
    // the filter says so, but a factory the table does not resolve to a
    // version would mean `get_lb_version_by_factory` and
    // `get_lb_factory_addresses_by_chain_id` disagree about the same table.
    for p in &pairs {
        assert!(
            p.factory_version.is_some(),
            "pair {} was emitted by {} which get_lb_factory_addresses_by_chain_id \
             returned but get_lb_version_by_factory does not resolve",
            p.address,
            p.factory
        );
    }

    Ok(pairs)
}

// ---------------------------------------------------------------------------
// 2. Triage
// ---------------------------------------------------------------------------

/// Probe one discovered pair with public getters only.
///
/// Deliberately cheaper than `fetch_lb_pool`: one version-detection multicall
/// plus one state multicall, no bin discovery. The point of triage is to
/// decide what is worth fetching, so it must not cost what fetching costs.
async fn probe_one<P: Provider + Send + Sync>(provider: &Arc<P>, address: Address) -> Triage {
    let block = BlockId::Number(BlockNumberOrTag::Latest);

    let version = match detect_lb_version(provider, address, MULTICALL, block).await {
        Ok(Some(v)) => v,
        Ok(None) => return Triage::NotLbPair,
        Err(e) => {
            return Triage::ProbeFailed {
                version: None,
                why: format!("detect_lb_version: {e:#}"),
            }
        }
    };

    match version {
        LBVersion::V2_1 | LBVersion::V2_2 => {
            let p = ILBPair::new(address, provider);
            let r = provider
                .multicall()
                .address(MULTICALL)
                .add(p.getBinStep())
                .add(p.getActiveId())
                .add(p.getReserves())
                .block(block)
                .try_aggregate(false)
                .await;
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    return Triage::ProbeFailed {
                        version: Some(version),
                        why: format!("state multicall: {e:#}"),
                    }
                }
            };
            match (r.0, r.1, r.2) {
                (Ok(bin_step), Ok(active_id), Ok(res)) => {
                    if res.reserveX == 0 && res.reserveY == 0 {
                        Triage::Empty { version }
                    } else {
                        Triage::Viable {
                            version,
                            active_id: active_id.to(),
                            bin_step,
                            reserve_x: res.reserveX,
                            reserve_y: res.reserveY,
                        }
                    }
                }
                (a, b, c) => Triage::ProbeFailed {
                    version: Some(version),
                    why: format!(
                        "getter reverted: binStep={} activeId={} reserves={}",
                        a.is_ok(),
                        b.is_ok(),
                        c.is_ok()
                    ),
                },
            }
        }
        LBVersion::V2_0 => {
            let p = ILBPairV20::new(address, provider);
            let r = provider
                .multicall()
                .address(MULTICALL)
                .add(p.getReservesAndId())
                .add(p.feeParameters())
                .block(block)
                .try_aggregate(false)
                .await;
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    return Triage::ProbeFailed {
                        version: Some(version),
                        why: format!("state multicall: {e:#}"),
                    }
                }
            };
            match (r.0, r.1) {
                (Ok(res), Ok(fee)) => {
                    // v2.0 reserves are uint256 on the wire; real bin totals
                    // fit u128, and a value that does not is itself a signal
                    // the pair is not what it claims to be.
                    let rx: u128 = res.reserveX.try_into().unwrap_or(u128::MAX);
                    let ry: u128 = res.reserveY.try_into().unwrap_or(u128::MAX);
                    if rx == 0 && ry == 0 {
                        Triage::Empty { version }
                    } else {
                        Triage::Viable {
                            version,
                            active_id: res.activeId.to(),
                            bin_step: fee.0,
                            reserve_x: rx,
                            reserve_y: ry,
                        }
                    }
                }
                (a, b) => Triage::ProbeFailed {
                    version: Some(version),
                    why: format!(
                        "getter reverted: getReservesAndId={} feeParameters={}",
                        a.is_ok(),
                        b.is_ok()
                    ),
                },
            }
        }
    }
}

/// Probe every discovered pair, in bounded-concurrency rounds.
async fn triage_all<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pairs: &[Discovered],
) -> Vec<(Discovered, Triage)> {
    let mut out = Vec::with_capacity(pairs.len());
    for chunk in pairs.chunks(PROBE_CONCURRENCY) {
        let futures: Vec<_> = chunk
            .iter()
            .map(|d| async move { (d.clone(), probe_one(provider, d.address).await) })
            .collect();
        out.extend(futures_util::future::join_all(futures).await);
    }
    out
}

// ---------------------------------------------------------------------------
// 3. Liveness
// ---------------------------------------------------------------------------

/// Count LB logs each address emitted over the last [`LIVENESS_BLOCKS`]
/// blocks. One `eth_getLogs` per address batch, filtered by the library's own
/// [`LBPool::topics`], so an address whose events this crate cannot decode
/// reads as dead — which, for the collector's purposes, it is.
async fn liveness<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    addresses: &[Address],
    head: u64,
) -> Result<HashMap<Address, usize>> {
    let mut counts: HashMap<Address, usize> = addresses.iter().map(|&a| (a, 0)).collect();
    let from = head.saturating_sub(LIVENESS_BLOCKS);
    // Batch the address filter: a 166-entry array in one call works on this
    // endpoint, but a smaller batch keeps the request well clear of any
    // provider-side cap.
    for batch in addresses.chunks(64) {
        let filter = Filter::new()
            .from_block(from)
            .to_block(head)
            .address(batch.to_vec())
            .event_signature(LBPool::topics());
        for log in provider.get_logs(&filter).await? {
            *counts.entry(log.address()).or_insert(0) += 1;
        }
    }
    Ok(counts)
}

// ---------------------------------------------------------------------------
// Collector helpers (same shape as lb_live_collector.rs)
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
/// `last_updated` is excluded on purpose — `apply_log` stamps it from wall
/// clock, so it moves whenever any event is applied and would make this gate
/// pass for reasons unrelated to the pool's state.
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
    let mut from = block_a + 1;
    while from <= block_b {
        let to = (from + GETLOGS_CHUNK - 1).min(block_b);
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
// The test
// ---------------------------------------------------------------------------

/// Polling mode (`use_websocket: false`). WebSocket needs `wss://`, which the
/// sandbox this was developed in blocks; `lb_live_collector.rs` covers that
/// path on curated fixtures.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_lb_discovery_e2e_polling() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .try_init();

    let soak = secs_from_env("LB_SOAK_SECS", DEFAULT_SOAK_SECS);
    let settle = secs_from_env("LB_SETTLE_SECS", DEFAULT_SETTLE_SECS);

    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let token_info = CachingTokenInfo::new();
    let fetch_config = build_fetch_config();

    // ── 1. Discover ─────────────────────────────────────────────────────
    let t0 = Instant::now();
    let discovered = discover(&provider).await?;
    println!("[discover] took {:?}", t0.elapsed());

    // ── 2. Triage ───────────────────────────────────────────────────────
    let t0 = Instant::now();
    let triaged = triage_all(&provider, &discovered).await;
    println!(
        "[triage] probed {} pair(s) in {:?}",
        triaged.len(),
        t0.elapsed()
    );

    let mut by_outcome: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_version: BTreeMap<String, usize> = BTreeMap::new();
    let mut failures: Vec<(Address, String)> = Vec::new();
    let mut version_mismatches: Vec<(Address, Option<LBVersion>, Option<LBVersion>)> = Vec::new();
    for (d, t) in &triaged {
        *by_outcome.entry(t.label()).or_default() += 1;
        *by_version.entry(format!("{:?}", t.version())).or_default() += 1;
        if let Triage::ProbeFailed { why, .. } = t {
            failures.push((d.address, why.clone()));
        }
        // The factory table says which generation an emitter deploys; the
        // on-chain discriminator says which generation the pair actually is.
        // They must agree, or `src/lb/factories.rs` is mislabelled and a
        // consumer trusting it would pick the wrong fetcher.
        if t.version().is_some() && t.version() != d.factory_version {
            version_mismatches.push((d.address, d.factory_version, t.version()));
        }
    }
    println!("[triage] outcomes:");
    for (k, v) in &by_outcome {
        println!("[triage]   {k}: {v}");
    }
    println!("[triage] detected versions:");
    for (k, v) in &by_version {
        println!("[triage]   {k}: {v}");
    }
    if !failures.is_empty() {
        println!("[triage] probe failures ({}):", failures.len());
        for (a, why) in failures.iter().take(20) {
            println!("[triage]   {a}: {why}");
        }
    }
    assert!(
        version_mismatches.is_empty(),
        "[triage] {} pair(s) disagree between the factory table in \
         src/lb/factories.rs and the on-chain discriminator: {:?}",
        version_mismatches.len(),
        version_mismatches
    );
    assert_eq!(
        by_outcome.get("not-an-lb-pair").copied().unwrap_or(0),
        0,
        "[triage] a pair decoded out of an LB factory's own LBPairCreated log \
         failed every LB version discriminator — either the ABI decodes the \
         LBPair field from the wrong position, or detect_lb_version has a hole"
    );

    // Distinct pairs sharing byte-identical state is the single most
    // surprising thing this window turns up, and it is *real chain state*, not
    // a decoding artifact — see the report in
    // `.superpowers/sdd/2026-08-21-lb-versioning/discovery-e2e-report.md`.
    // Reported rather than asserted on: the cluster size is a property of what
    // one launchpad happened to mint, not an invariant. It is printed so that
    // a future run where the cluster *vanishes* (pairs started trading) or
    // where an unrelated set of pairs suddenly collapses into one state (a
    // fetcher reading the wrong contract) is visible in the output.
    let mut state_clusters: BTreeMap<String, usize> = BTreeMap::new();
    for (_, t) in &triaged {
        if let Triage::Viable {
            version,
            active_id,
            reserve_x,
            reserve_y,
            ..
        } = t
        {
            *state_clusters
                .entry(format!(
                    "{version:?} activeId={active_id} reserves={reserve_x}/{reserve_y}"
                ))
                .or_default() += 1;
        }
    }
    let mut clusters: Vec<(&String, &usize)> = state_clusters.iter().collect();
    clusters.sort_by(|a, b| b.1.cmp(a.1));
    println!(
        "[triage] {} distinct (version, activeId, reserves) state(s) across {} viable pair(s); \
         largest clusters:",
        clusters.len(),
        clusters.iter().map(|(_, n)| **n).sum::<usize>()
    );
    for (state, n) in clusters.iter().take(3) {
        println!("[triage]   x{n}: {state}");
    }

    // ── 3. Liveness, and the pre-soak anti-vacuity gate ─────────────────
    let head = provider.get_block_number().await?;
    let all_addresses: Vec<Address> = discovered.iter().map(|d| d.address).collect();
    let activity = liveness(&provider, &all_addresses, head).await?;
    let live_count = activity.values().filter(|&&n| n > 0).count();
    let total_recent_logs: usize = activity.values().sum();
    println!(
        "[liveness] {live_count}/{} discovered pair(s) emitted at least one LB log in \
         blocks {}..={head} ({total_recent_logs} logs total)",
        all_addresses.len(),
        head.saturating_sub(LIVENESS_BLOCKS)
    );
    assert!(
        live_count > 0,
        "[liveness] not one of the {} discovered pairs emitted an LB log in the \
         last {LIVENESS_BLOCKS} blocks. Every pool discovery turned up is dead, \
         so a collector soak over them cannot move any state and the \
         convergence assertion would compare two identical snapshots. This run \
         cannot exercise anything and must not report a pass — re-pin WINDOWS \
         onto an era whose pairs still trade.",
        all_addresses.len()
    );

    // ── 4. Sample, by a mechanical rule ─────────────────────────────────
    // Rank the viable pairs by measured recent activity, descending, ties
    // broken by address so the choice is reproducible. Then force up to
    // EMPTY_SAMPLE_SLOTS empty pairs in, because the ranking would never pick
    // one and they are the common case in the wild.
    let mut viable: Vec<&(Discovered, Triage)> = triaged
        .iter()
        .filter(|(_, t)| matches!(t, Triage::Viable { .. }))
        .collect();
    viable.sort_by(|a, b| {
        let ka = activity.get(&a.0.address).copied().unwrap_or(0);
        let kb = activity.get(&b.0.address).copied().unwrap_or(0);
        kb.cmp(&ka).then(a.0.address.cmp(&b.0.address))
    });
    let empties: Vec<&(Discovered, Triage)> = triaged
        .iter()
        .filter(|(_, t)| matches!(t, Triage::Empty { .. }))
        .take(EMPTY_SAMPLE_SLOTS)
        .collect();

    let viable_slots = SAMPLE_SIZE.saturating_sub(empties.len());
    let mut sample: Vec<&(Discovered, Triage)> = Vec::with_capacity(SAMPLE_SIZE);
    let mut taken: std::collections::HashSet<Address> = std::collections::HashSet::new();

    // Generation coverage first. Every generation discovery turned up must
    // reach the collector, or the run silently stops covering whichever one
    // happens to be quiet that day: v2.0 pairs trade rarely enough that pure
    // activity ranking drops them most runs, and v2.0 is the generation with
    // its own state getters, its own event ABI, and no corroborated bin-tree
    // layout — i.e. the one most worth exercising. `viable` is already sorted
    // by activity, so `find` takes the most active pair of each generation.
    let mut covered_generations = Vec::new();
    for generation in [LBVersion::V2_0, LBVersion::V2_1, LBVersion::V2_2] {
        if sample.len() >= viable_slots {
            break;
        }
        if let Some(entry) = viable.iter().find(|(_, t)| t.version() == Some(generation)) {
            if taken.insert(entry.0.address) {
                sample.push(entry);
                covered_generations.push(generation);
            }
        }
    }
    // Then fill by activity.
    for entry in &viable {
        if sample.len() >= viable_slots {
            break;
        }
        if taken.insert(entry.0.address) {
            sample.push(entry);
        }
    }
    sample.extend(empties.iter().copied());

    println!(
        "[sample] {} of {} discovered pair(s): one most-active pair per discovered \
         generation ({covered_generations:?}), then the rest of the {viable_slots} \
         viable slots by LB logs in the last {LIVENESS_BLOCKS} blocks (ties by \
         address), plus {} empty pair(s) forced in to exercise the degenerate path",
        sample.len(),
        discovered.len(),
        empties.len()
    );
    for (d, t) in &sample {
        // `binStep` here is the value decoded out of the creation log; the
        // parenthesised one is what the pair reports on chain now. They are
        // two independent reads of the same fact, so printing both makes an
        // ABI field-order mistake visible in the output rather than only in a
        // downstream assertion.
        let (probe_bin_step, active_id, reserves) = match t {
            Triage::Viable {
                bin_step,
                active_id,
                reserve_x,
                reserve_y,
                ..
            } => (
                bin_step.to_string(),
                active_id.to_string(),
                format!("{reserve_x}/{reserve_y}"),
            ),
            _ => ("-".to_string(), "-".to_string(), "0/0".to_string()),
        };
        println!(
            "[sample]   {} {:?} binStep={}({}) {}<>{} window={} created={} recentLogs={} \
             activeId={} reserves={} triage={}",
            d.address,
            t.version(),
            d.bin_step,
            probe_bin_step,
            d.token_x,
            d.token_y,
            d.window,
            d.block,
            activity.get(&d.address).copied().unwrap_or(0),
            active_id,
            reserves,
            t.label()
        );
    }
    let sample_addresses: Vec<Address> = sample.iter().map(|(d, _)| d.address).collect();
    assert!(
        sample_addresses
            .iter()
            .any(|a| activity.get(a).copied().unwrap_or(0) > 0),
        "[sample] the sample contains no pair with recent activity"
    );
    for generation in [LBVersion::V2_0, LBVersion::V2_1, LBVersion::V2_2] {
        let discovered_any = viable.iter().any(|(_, t)| t.version() == Some(generation));
        let sampled_any = sample.iter().any(|(_, t)| t.version() == Some(generation));
        assert!(
            !discovered_any || sampled_any,
            "[sample] discovery found viable {generation:?} pair(s) but none reached \
             the collector, so this run never exercises that generation's fetcher \
             or event ABI"
        );
    }

    // ── 5. Fetch the sample into a registry ─────────────────────────────
    let live = Arc::new(PoolRegistry::new(CHAIN_ID));
    let block_a = provider.get_block_number().await?;
    println!("[e2e] block_a = {block_a}");

    let t0 = Instant::now();
    let fetched = fetch_pools_into_registry(
        &provider,
        &sample_addresses,
        BlockNumberOrTag::Number(block_a),
        &token_info,
        &live,
        &fetch_config,
    )
    .await?;
    println!(
        "[e2e] fetched {}/{} sampled pool(s) in {:?}",
        fetched.len(),
        sample_addresses.len(),
        t0.elapsed()
    );
    assert_eq!(
        live.pool_count(),
        sample_addresses.len(),
        "[e2e] not every sampled pair made it into the registry"
    );

    // ── 6. Snapshot, and check triage predicted the fetch ───────────────
    let mut snapshots: Vec<(Address, LBPool)> = Vec::new();
    let mut empty_after_fetch = 0usize;
    for (d, t) in &sample {
        let pool = lb_pool_from_registry(&live, d.address).await;
        println!(
            "[e2e] snapshot {} version={:?} binStep={} active_id={} bins={} hooks={}",
            d.address,
            pool.version,
            pool.bin_step,
            pool.active_id,
            pool.bins.len(),
            pool.hooks_parameters.is_some()
        );
        if pool.bins.is_empty() {
            empty_after_fetch += 1;
        }
        // Triage says "empty" from reserves alone; the fetcher says it from
        // the bin tree. If those two ever disagree the cheap probe is no
        // longer a valid stand-in for a fetch, which is the whole basis of
        // bounding the sample.
        match t {
            Triage::Empty { .. } => assert!(
                pool.bins.is_empty(),
                "[e2e] {} triaged empty (zero reserves) but fetched {} bin(s)",
                d.address,
                pool.bins.len()
            ),
            Triage::Viable { .. } => assert!(
                !pool.bins.is_empty(),
                "[e2e] {} triaged viable (non-zero reserves) but fetched zero bins",
                d.address
            ),
            _ => {}
        }
        snapshots.push((d.address, pool));
    }
    println!("[e2e] {empty_after_fetch} sampled pool(s) fetched with zero bins");

    // ── 7. Run the real collector ───────────────────────────────────────
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

    println!("[e2e] collector running — soaking {soak}s ...");
    tokio::time::sleep(Duration::from_secs(soak)).await;
    println!(
        "[e2e] soak done, last_processed_block = {}",
        live.get_last_processed_block()
    );
    println!("[e2e] settling {settle}s before stop ...");
    tokio::time::sleep(Duration::from_secs(settle)).await;
    handle.stop().await;

    let block_b = live.get_last_processed_block();
    println!("[e2e] block_b = {block_b} ({} blocks)", block_b - block_a);
    assert!(
        block_b > block_a,
        "[e2e] collector never advanced past block_a ({block_a})"
    );

    let counts = count_logs_per_pool(&provider, &sample_addresses, block_a, block_b).await?;
    let total_logs: usize = counts.values().sum();
    for a in &sample_addresses {
        println!(
            "[e2e] on-chain logs in ({block_a}, {block_b}] for {a}: {}",
            counts.get(a).copied().unwrap_or(0)
        );
    }
    println!("[e2e] total logs over the soak window: {total_logs}");

    // ── 8. Post-soak anti-vacuity gate ──────────────────────────────────
    let mut moved: Vec<Address> = Vec::new();
    let mut post: Vec<(Address, LBPool)> = Vec::new();
    for (addr, before) in &snapshots {
        let after = lb_pool_from_registry(&live, *addr).await;
        match describe_movement(before, &after) {
            Some(what) => {
                println!("[e2e] {addr} MOVED: {what}");
                moved.push(*addr);
            }
            None => println!("[e2e] {addr} unchanged over the soak"),
        }
        post.push((*addr, after));
    }
    assert!(
        !moved.is_empty(),
        "[e2e] the soak window caught no activity: not one of the {} sampled \
         pools changed its active_id or bins over blocks {block_a}..={block_b} \
         ({total_logs} LB logs on chain in that window). The convergence \
         assertions below would be comparing two identical states and proving \
         nothing. Raise LB_SOAK_SECS.",
        sample_addresses.len()
    );

    // ── 9. Fresh, independent fetch at block B ──────────────────────────
    let fresh = Arc::new(PoolRegistry::new(CHAIN_ID));
    let t0 = Instant::now();
    fetch_pools_into_registry(
        &provider,
        &sample_addresses,
        BlockNumberOrTag::Number(block_b),
        &token_info,
        &fresh,
        &fetch_config,
    )
    .await?;
    println!("[e2e] fresh fetch at {block_b} took {:?}", t0.elapsed());

    // ── 10. Collector-built state must equal the fresh fetch ────────────
    for (addr, live_pool) in &post {
        let fresh_pool = lb_pool_from_registry(&fresh, *addr).await;
        assert_lb_pools_converge(live_pool, &fresh_pool, &format!("discovered/{addr}"));
    }
    println!(
        "[e2e] all {} sampled pool(s) converged with a fresh fetch at {block_b}",
        post.len()
    );

    println!(
        "[e2e] PASSED — discovered {} pairs, sampled {}, blocks {block_a}..={block_b}, \
         {total_logs} logs, {} pool(s) moved",
        discovered.len(),
        sample_addresses.len(),
        moved.len()
    );
    Ok(())
}
