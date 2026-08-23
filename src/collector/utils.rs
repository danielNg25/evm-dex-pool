use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, FixedBytes};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use anyhow::Result;
use log::warn;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
pub async fn fetch_events<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    addresses: Vec<Address>,
    topics: Vec<FixedBytes<32>>,
    from_block: BlockNumberOrTag,
    to_block: BlockNumberOrTag,
) -> Result<Vec<Log>> {
    let filter = Filter::new()
        .from_block(from_block)
        .to_block(to_block)
        .address(addresses)
        .event_signature(topics);

    let events = provider.get_logs(&filter).await?;
    Ok(events)
}

/// Distinct block numbers among logs that still lack a timestamp.
///
/// Deduplicated so one header fetch serves every log in that block.
fn blocks_needing_timestamps(logs: &[Log]) -> Vec<u64> {
    logs.iter()
        .filter(|l| l.block_timestamp.is_none())
        .filter_map(|l| l.block_number)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Populate `block_timestamp` on logs whose RPC response omitted it.
///
/// `blockTimestamp` is not part of a standard `eth_getLogs` response and many
/// endpoints — Avalanche's among them — never send it, so logs arrive with
/// `block_timestamp: None`. LB pools need chain time to reproduce the
/// contract's volatility decay, and wall clock is not a substitute: it makes
/// event-replayed state diverge from freshly-fetched state permanently.
///
/// Fetches one header per distinct block, concurrently. Logs whose block
/// header cannot be read are left with `None`, so callers keep whatever
/// fallback they already have rather than getting a wrong timestamp.
pub async fn enrich_log_timestamps<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    logs: &mut [Log],
) -> Result<()> {
    let wanted = blocks_needing_timestamps(logs);
    if wanted.is_empty() {
        return Ok(());
    }

    let futures = wanted.iter().map(|&n| {
        let provider = provider.clone();
        async move {
            match provider.get_block_by_number(BlockNumberOrTag::Number(n)).await {
                Ok(Some(block)) => (n, Ok(block.header.timestamp)),
                // The RPC call succeeded but had nothing to return for this
                // number (e.g. reorg'd away, or not yet visible to this node).
                Ok(None) => (n, Err("no block returned".to_string())),
                // The RPC call itself failed (timeout, rate limit, transport
                // error, etc). Distinct from the above: this is a request
                // that never got an answer at all.
                Err(e) => (n, Err(e.to_string())),
            }
        }
    });

    let mut fetched: HashMap<u64, u64> = HashMap::new();
    let mut failures: Vec<(u64, String)> = Vec::new();
    for (n, result) in futures_util::future::join_all(futures).await {
        match result {
            Ok(ts) => {
                fetched.insert(n, ts);
            }
            Err(reason) => failures.push((n, reason)),
        }
    }

    // A block whose header could not be read leaves every log in it on the
    // caller's fallback (wall clock, for `LBPool::apply_log`). That fallback
    // now feeds `update_references`'s `dt` calculation directly, so silent
    // degradation here can zero out `volatility_reference` mid-replay. Loud
    // by design: this is exactly the failure mode the timestamp-enrichment
    // path exists to prevent.
    if !failures.is_empty() {
        const MAX_REASONS_SHOWN: usize = 5;
        let reasons = failures
            .iter()
            .take(MAX_REASONS_SHOWN)
            .map(|(n, reason)| format!("{n}: {reason}"))
            .collect::<Vec<_>>()
            .join(", ");
        let remaining = failures.len().saturating_sub(MAX_REASONS_SHOWN);
        let more = if remaining > 0 {
            format!(" (+{remaining} more)")
        } else {
            String::new()
        };
        warn!(
            "enrich_log_timestamps: resolved {} of {} requested block headers; \
             logs in unresolved blocks keep their wall-clock fallback timestamp; \
             failures: [{reasons}]{more}",
            fetched.len(),
            wanted.len()
        );
    }

    for log in logs.iter_mut() {
        if log.block_timestamp.is_none() {
            if let Some(n) = log.block_number {
                log.block_timestamp = fetched.get(&n).copied();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, LogData};

    fn log_at(block: u64) -> Log {
        Log {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(vec![], Default::default()),
            },
            block_number: Some(block),
            block_timestamp: None,
            ..Default::default()
        }
    }

    /// A log that already carries a timestamp must be left alone, and one
    /// without a block number cannot be enriched — neither should panic.
    #[test]
    fn enrichment_targets_only_logs_that_need_it() {
        let mut logs = vec![log_at(100), log_at(100), log_at(101)];
        logs[0].block_timestamp = Some(1_700_000_000);

        let need: Vec<u64> = blocks_needing_timestamps(&logs);
        // Block 100 still needs it (logs[1]), 101 needs it, and the set is
        // deduplicated so one header fetch serves both logs at block 100.
        assert_eq!(need, vec![100, 101]);

        let mut none_needed = vec![log_at(7)];
        none_needed[0].block_timestamp = Some(42);
        assert!(blocks_needing_timestamps(&none_needed).is_empty());
    }

    /// Exercises `enrich_log_timestamps` against a real `Provider` backed by
    /// alloy's built-in mock transport (`alloy::providers::mock::Asserter`),
    /// so the RPC-error and no-block branches run as actual code paths
    /// rather than being inferred from `blocks_needing_timestamps` alone.
    mod with_mocked_provider {
        use super::*;
        use alloy::providers::mock::Asserter;
        use alloy::providers::ProviderBuilder;
        use alloy::rpc::types::{Block, Header as RpcHeader};

        fn block_with_timestamp(timestamp: u64) -> Block {
            let inner = alloy::consensus::Header {
                timestamp,
                ..Default::default()
            };
            Block::empty(RpcHeader::new(inner))
        }

        /// Happy path sanity check: confirms the mocked provider actually
        /// exercises `get_block_by_number` the way the live RPC path does,
        /// so the failure-mode tests below are trustworthy.
        #[tokio::test]
        async fn fills_in_timestamp_when_the_header_is_returned() {
            let asserter = Asserter::new();
            let provider = Arc::new(ProviderBuilder::new().connect_mocked_client(asserter.clone()));
            asserter.push_success(&block_with_timestamp(1_700_000_000));

            let mut logs = vec![log_at(100)];
            enrich_log_timestamps(&provider, &mut logs).await.unwrap();

            assert_eq!(logs[0].block_timestamp, Some(1_700_000_000));
        }

        /// Failure mode 1: the RPC call itself errors out.
        #[tokio::test]
        async fn keeps_fallback_when_the_rpc_call_errors() {
            let asserter = Asserter::new();
            let provider = Arc::new(ProviderBuilder::new().connect_mocked_client(asserter.clone()));
            asserter.push_failure_msg("boom: connection reset");

            let mut logs = vec![log_at(100)];
            let result = enrich_log_timestamps(&provider, &mut logs).await;

            // Same contract as before: partial failure still returns Ok(()),
            // and the unresolved log keeps its caller-supplied fallback.
            assert!(result.is_ok());
            assert_eq!(logs[0].block_timestamp, None);
        }

        /// Failure mode 2: the RPC call succeeds but has no block for that
        /// number (e.g. `eth_getBlockByNumber` returning `null`).
        #[tokio::test]
        async fn keeps_fallback_when_no_block_is_returned() {
            let asserter = Asserter::new();
            let provider = Arc::new(ProviderBuilder::new().connect_mocked_client(asserter.clone()));
            asserter.push_success(&Option::<Block>::None);

            let mut logs = vec![log_at(100)];
            let result = enrich_log_timestamps(&provider, &mut logs).await;

            assert!(result.is_ok());
            assert_eq!(logs[0].block_timestamp, None);
        }
    }
}
