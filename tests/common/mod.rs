//! Shared helpers for LB pool event-replay convergence tests.
//!
//! Each file directly under `tests/` compiles as its own independent test
//! binary, so anything reused across LB convergence fixtures — this one,
//! plus the v2.0 fixture added later — lives here instead of being
//! duplicated. A subdirectory (`tests/common/mod.rs`, not `tests/common.rs`)
//! is required: cargo only auto-discovers top-level files in `tests/` as
//! test binaries, so this module is invisible to that discovery and must be
//! pulled in explicitly with `mod common;`.

#![cfg(feature = "collector")]

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{address, Address, B256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{Filter, Log};
use anyhow::Result;

use evm_dex_pool::collector::enrich_log_timestamps;
use evm_dex_pool::lb::{fetch_lb_pool, LBPool};
use evm_dex_pool::{EventApplicable, TokenInfo, TopicList};

pub const RPC_URL: &str = "https://api.avax.network/ext/bc/C/rpc";
pub const CHAIN_ID: u64 = 43114;
pub const MULTICALL: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

/// How many blocks a head-derived replay window spans, when a fixture does
/// not pin an explicit range. Wide enough to usually catch real swap
/// activity, but not guaranteed to: even a normally busy pool has been
/// observed with zero logs in a head-derived 2000-block window, because
/// trading activity varies over time. Don't infer a steady log rate from
/// this constant — nobody has re-measured one since it last changed, and a
/// stale rate is actively misleading about whether a quiet window is
/// expected.
///
/// Kept just under the endpoint's 2048-block `eth_getLogs` cap so the log
/// fetch below stays a single call. Widening past 2048 requires chunking it.
pub const REPLAY_BLOCKS: u64 = 2000;

pub struct CachingTokenInfo {
    cache: Arc<Mutex<HashMap<Address, (Address, u8)>>>,
}

impl CachingTokenInfo {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for CachingTokenInfo {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenInfo for CachingTokenInfo {
    fn get_or_fetch_token<P: Provider + Send + Sync>(
        &self,
        _provider: &Arc<P>,
        address: Address,
        _multicall_address: Address,
    ) -> impl Future<Output = Result<(Address, u8)>> + Send {
        let cache = Arc::clone(&self.cache);
        async move {
            let mut guard = cache.lock().unwrap();
            let entry = guard.entry(address).or_insert((address, 18u8));
            Ok(*entry)
        }
    }
}

/// Compare every field that mirrors on-chain state.
///
/// `last_updated` and `created_at` are excluded — they are local
/// bookkeeping, not chain state.
pub fn assert_lb_pools_converge(replayed: &LBPool, fetched: &LBPool, label: &str) {
    assert_eq!(replayed.version, fetched.version, "{label}: version");
    assert_eq!(replayed.bin_step, fetched.bin_step, "{label}: bin_step");
    assert_eq!(replayed.active_id, fetched.active_id, "{label}: active_id");

    assert_eq!(
        replayed.base_factor, fetched.base_factor,
        "{label}: base_factor"
    );
    assert_eq!(
        replayed.filter_period, fetched.filter_period,
        "{label}: filter_period"
    );
    assert_eq!(
        replayed.decay_period, fetched.decay_period,
        "{label}: decay_period"
    );
    assert_eq!(
        replayed.reduction_factor, fetched.reduction_factor,
        "{label}: reduction_factor"
    );
    assert_eq!(
        replayed.variable_fee_control, fetched.variable_fee_control,
        "{label}: variable_fee_control"
    );
    assert_eq!(
        replayed.protocol_share, fetched.protocol_share,
        "{label}: protocol_share"
    );
    assert_eq!(
        replayed.max_volatility_accumulator, fetched.max_volatility_accumulator,
        "{label}: max_volatility_accumulator"
    );

    assert_eq!(
        replayed.volatility_accumulator, fetched.volatility_accumulator,
        "{label}: volatility_accumulator"
    );
    assert_eq!(
        replayed.volatility_reference, fetched.volatility_reference,
        "{label}: volatility_reference"
    );
    assert_eq!(
        replayed.id_reference, fetched.id_reference,
        "{label}: id_reference"
    );
    assert_eq!(
        replayed.time_of_last_update, fetched.time_of_last_update,
        "{label}: time_of_last_update"
    );

    // Bins are compared in full. Both sides already drop zero-reserve bins:
    // update_bin removes them, and the fetcher only inserts non-zero ones.
    assert_eq!(
        replayed.bins.len(),
        fetched.bins.len(),
        "{label}: bin count — replayed {} vs fetched {}",
        replayed.bins.len(),
        fetched.bins.len()
    );
    for (id, fetched_reserves) in &fetched.bins {
        let replayed_reserves = replayed.bins.get(id).unwrap_or_else(|| {
            panic!("{label}: bin {id} present after fetch but missing from replay")
        });
        assert_eq!(
            replayed_reserves, fetched_reserves,
            "{label}: bin {id} reserves"
        );
    }
}

/// Assert the replayed range actually contains every event kind named.
///
/// Without this a quiet or drifted range yields a green test that exercised
/// nothing — which has already happened twice on this branch. Prints the
/// per-topic counts so a shrinking fixture is visible before it hits zero.
pub fn assert_range_covers(logs: &[Log], required: &[(&str, B256)], label: &str) {
    for (name, topic) in required {
        let n = logs.iter().filter(|l| l.topic0() == Some(topic)).count();
        println!("[{label}] coverage: {name} = {n}");
        assert!(
            n > 0,
            "{label}: pinned range contains no {name} event, so this test \
             would pass without ever exercising it. Re-pin the range."
        );
    }
}

/// `pinned` selects the block range. `None` derives it from the chain head,
/// which suits busy pools but makes the test a canary, not a gate — see
/// each caller's doc comment for why. `Some((a, b))` uses a fixed historical
/// range. Archive `eth_call` is verified working 10,000,000 blocks back on
/// this endpoint, so pinned ranges resolve.
///
/// `required` names the event topics the replayed range must contain — see
/// `assert_range_covers`. Taken as a parameter rather than hardcoded so
/// different fixture generations (e.g. v2.0) can name their own set.
pub async fn converge_one(
    pool_address: Address,
    label: &str,
    pinned: Option<(u64, u64)>,
    required: &[(&str, B256)],
) -> Result<()> {
    let provider = Arc::new(ProviderBuilder::new().connect_http(RPC_URL.parse()?));
    let token_info = CachingTokenInfo::new();

    let (block_a, block_b) = match pinned {
        Some(range) => range,
        None => {
            let head = provider.get_block_number().await?;
            (head - REPLAY_BLOCKS, head)
        }
    };

    println!("[{label}] replaying {pool_address} from {block_a} to {block_b}");

    // 1. Fetch at block A.
    let mut replayed = fetch_lb_pool(
        &provider,
        pool_address,
        BlockId::Number(BlockNumberOrTag::Number(block_a)),
        &token_info,
        MULTICALL,
        CHAIN_ID,
    )
    .await?;

    // 2. Replay every LB log in (A, B], fetched in <=2000-block chunks — this
    // endpoint rejects eth_getLogs ranges wider than 2048 blocks, and pinned
    // ranges here run up to 11,000.
    const CHUNK: u64 = 2000;
    let mut logs = Vec::new();
    let mut from = block_a + 1;
    while from <= block_b {
        let to = (from + CHUNK - 1).min(block_b);
        let filter = Filter::new()
            .from_block(from)
            .to_block(to)
            .address(pool_address)
            .event_signature(LBPool::topics());
        logs.extend(provider.get_logs(&filter).await?);
        from = to + 1;
    }
    println!("[{label}] applying {} logs", logs.len());
    assert!(
        !logs.is_empty(),
        "{label}: no logs in range — widen REPLAY_BLOCKS"
    );

    assert_range_covers(&logs, required, label);

    // Required, not optional: `eth_getLogs` does not return `blockTimestamp`,
    // so every log arrives with `block_timestamp: None` and `apply_log` would
    // fall back to wall clock — guaranteeing `time_of_last_update` diverges
    // from the refetched value and failing this test for the wrong reason.
    // This is the same helper the collector's BlockSource uses (Task 5b).
    enrich_log_timestamps(&provider, &mut logs).await?;
    assert!(
        logs.iter().all(|l| l.block_timestamp.is_some()),
        "{label}: some logs still lack a block timestamp after enrichment"
    );

    for log in &logs {
        replayed.apply_log(log)?;
    }

    // 3. Fetch fresh at block B.
    let fetched = fetch_lb_pool(
        &provider,
        pool_address,
        BlockId::Number(BlockNumberOrTag::Number(block_b)),
        &token_info,
        MULTICALL,
        CHAIN_ID,
    )
    .await?;

    // 4. They must be identical.
    assert_lb_pools_converge(&replayed, &fetched, label);
    println!("[{label}] converged across {} bins", fetched.bins.len());
    Ok(())
}
