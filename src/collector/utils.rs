use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, FixedBytes};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use anyhow::Result;
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
            let block = provider
                .get_block_by_number(BlockNumberOrTag::Number(n))
                .await
                .ok()
                .flatten();
            (n, block.map(|b| b.header.timestamp))
        }
    });
    let fetched: HashMap<u64, u64> = futures_util::future::join_all(futures)
        .await
        .into_iter()
        .filter_map(|(n, ts)| ts.map(|t| (n, t)))
        .collect();

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
}
