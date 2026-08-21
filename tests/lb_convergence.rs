//! Event-replay convergence: state built by applying logs from block A to
//! block B must equal state fetched fresh at block B.
//!
//! ```bash
//! cargo test --features collector test_lb_convergence -- --ignored --nocapture
//! ```

#![cfg(feature = "collector")]

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{address, Address};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use anyhow::Result;

use evm_dex_pool::collector::enrich_log_timestamps;
use evm_dex_pool::lb::{fetch_lb_pool, LBPool};
use evm_dex_pool::{EventApplicable, TokenInfo, TopicList};

const RPC_URL: &str = "https://api.avax.network/ext/bc/C/rpc";
const CHAIN_ID: u64 = 43114;
const MULTICALL: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

/// How many blocks to replay. Wide enough to capture real swap activity.
///
/// 500 was measured to be too narrow: the v2.2 fixture produced zero logs in a
/// head-derived 500-block window, aborting the test before it compared
/// anything. At 2000 blocks the fixtures yield ~27 (v2.1) and ~9 (v2.2) logs.
///
/// Kept just under the endpoint's 2048-block `eth_getLogs` cap so the log
/// fetch below stays a single call. Widening past 2048 requires chunking it.
const REPLAY_BLOCKS: u64 = 2000;

const V22_POOL: Address = address!("8573f98175d816d520248b5facf40d309b1c9cee");
const V21_POOL: Address = address!("4224f6f4c9280509724db2dbac314621e4465c29");

struct CachingTokenInfo {
    cache: Arc<Mutex<HashMap<Address, (Address, u8)>>>,
}

impl CachingTokenInfo {
    fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
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

/// `pinned` selects the block range. `None` derives it from the chain head,
/// which suits busy pools. `Some((a, b))` uses a fixed historical range —
/// required for pools too quiet to guarantee logs near the head (see the
/// v2.0 fixture in Task 11). Archive `eth_call` is verified working at
/// 50,000 blocks back on this endpoint, so pinned ranges resolve.
async fn converge_one(
    pool_address: Address,
    label: &str,
    pinned: Option<(u64, u64)>,
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

    // 2. Replay every LB log in (A, B].
    let filter = Filter::new()
        .from_block(block_a + 1)
        .to_block(block_b)
        .address(pool_address)
        .event_signature(LBPool::topics());
    let mut logs = provider.get_logs(&filter).await?;
    println!("[{label}] applying {} logs", logs.len());
    assert!(
        !logs.is_empty(),
        "{label}: no logs in range — widen REPLAY_BLOCKS"
    );

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

#[tokio::test]
#[ignore]
async fn test_lb_convergence_v22() -> Result<()> {
    // Busy pool: ~20 logs per 2000 blocks. Head-derived range is fine.
    converge_one(V22_POOL, "v2.2", None).await
}

#[tokio::test]
#[ignore]
async fn test_lb_convergence_v21() -> Result<()> {
    // Busy pool: ~49 logs per 2000 blocks.
    converge_one(V21_POOL, "v2.1", None).await
}
