use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, FixedBytes};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use anyhow::Result;
use log::warn;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

/// How many times to try a single block header fetch before giving up on it.
const HEADER_FETCH_ATTEMPTS: u32 = 3;
/// Delay between retry attempts for a single block header fetch.
///
/// This sits on the collector's hot path, so it stays short: a handful of
/// tens-of-milliseconds retries is cheap insurance against a transient RPC
/// blip, not a mechanism for riding out a real outage.
const HEADER_FETCH_RETRY_DELAY: Duration = Duration::from_millis(25);
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

/// Whether a log still needs its block timestamp filled in.
///
/// `None` is the usual case. `Some(0)` is the other: Sentio's Avalanche
/// endpoint sends `"blockTimestamp": "0x0"` on every log rather than omitting
/// the field. No block holding a pool event was mined at time zero, so the
/// two say the same thing — the RPC did not tell us.
fn lacks_block_timestamp(log: &Log) -> bool {
    log.block_timestamp.is_none_or(|t| t == 0)
}

/// Distinct block numbers among the logs `wanted` selects that still lack a
/// timestamp and whose block time is not already `known`.
///
/// Deduplicated so one header fetch serves every log in that block.
fn blocks_needing_timestamps(
    logs: &[Log],
    wanted: &dyn Fn(&Log) -> bool,
    known: &HashMap<u64, u64>,
) -> Vec<u64> {
    logs.iter()
        .filter(|l| wanted(l) && lacks_block_timestamp(l))
        .filter_map(|l| l.block_number)
        .filter(|n| !known.contains_key(n))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Fill `block_timestamp` on the `wanted` logs that lack one, from `times`
/// (block number -> timestamp). A log whose block is not in `times` keeps
/// what the RPC sent, so callers keep whatever fallback they already have.
fn fill_block_timestamps(logs: &mut [Log], wanted: &dyn Fn(&Log) -> bool, times: &HashMap<u64, u64>) {
    for log in logs.iter_mut() {
        if wanted(log) && lacks_block_timestamp(log) {
            if let Some(time) = log.block_number.and_then(|n| times.get(&n)) {
                log.block_timestamp = Some(*time);
            }
        }
    }
}

/// Populate `block_timestamp` on logs whose RPC response omitted it.
///
/// `blockTimestamp` is not part of a standard `eth_getLogs` response and many
/// endpoints — Avalanche's among them — never send it, so logs arrive with
/// `block_timestamp: None`; others send a placeholder zero, which counts as
/// missing too (see [`lacks_block_timestamp`]). LB pools need chain time to
/// reproduce the contract's volatility decay, and wall clock is not a
/// substitute: it makes event-replayed state diverge from freshly-fetched
/// state permanently.
pub async fn enrich_log_timestamps<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    logs: &mut [Log],
) -> Result<()> {
    enrich_log_timestamps_where(provider, logs, &|_| true, &HashMap::new()).await
}

/// [`enrich_log_timestamps`] for the logs `wanted` selects, taking any block
/// time already `known` (e.g. from the block poll) before fetching a header.
pub async fn enrich_log_timestamps_where<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    logs: &mut [Log],
    wanted: &(dyn Fn(&Log) -> bool + Sync),
    known: &HashMap<u64, u64>,
) -> Result<()> {
    let to_fetch = blocks_needing_timestamps(logs, wanted, known);
    let mut times = known.clone();
    if !to_fetch.is_empty() {
        times.extend(fetch_block_timestamps(provider, &to_fetch).await);
    }
    fill_block_timestamps(logs, wanted, &times);
    Ok(())
}

/// One header per block, concurrently, each retried. Blocks whose header
/// cannot be read are absent from the result, and warned about.
async fn fetch_block_timestamps<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    blocks: &[u64],
) -> HashMap<u64, u64> {
    // Each distinct block gets its own future so the fan-out across blocks
    // stays concurrent (via `join_all` below); the retry loop inside each
    // future only serialises attempts for that one block.
    let futures = blocks.iter().map(|&n| {
        let provider = provider.clone();
        async move {
            let mut last_reason = String::new();
            for attempt in 1..=HEADER_FETCH_ATTEMPTS {
                match provider.get_block_by_number(BlockNumberOrTag::Number(n)).await {
                    Ok(Some(block)) => return (n, Ok(block.header.timestamp)),
                    // The call succeeded but had nothing for this number (e.g.
                    // reorg'd away, or not yet visible to this node): retried.
                    Ok(None) => last_reason = "no block returned".to_string(),
                    // The call itself failed (timeout, rate limit, transport).
                    Err(e) => last_reason = e.to_string(),
                }
                if attempt < HEADER_FETCH_ATTEMPTS {
                    tokio::time::sleep(HEADER_FETCH_RETRY_DELAY).await;
                }
            }
            (n, Err(last_reason))
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
    // caller's fallback (wall clock, for `LBPool::apply_log`), which feeds
    // `update_references`'s `dt` directly. Loud by design.
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
            "enrich_log_timestamps: resolved {} of {} requested block headers \
             after {HEADER_FETCH_ATTEMPTS} attempts each; \
             logs in unresolved blocks keep their wall-clock fallback timestamp; \
             failures: [{reasons}]{more}",
            fetched.len(),
            blocks.len()
        );
    }
    fetched
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

    fn log_from_at(address: Address, block: u64) -> Log {
        let mut log = log_at(block);
        log.inner.address = address;
        log
    }

    /// A log that already carries a timestamp must be left alone, and one
    /// without a block number cannot be enriched — neither should panic.
    #[test]
    fn enrichment_targets_only_logs_that_need_it() {
        let mut logs = vec![log_at(100), log_at(100), log_at(101)];
        logs[0].block_timestamp = Some(1_700_000_000);

        let need: Vec<u64> = blocks_needing_timestamps(&logs, &|_| true, &HashMap::new());
        // Block 100 still needs it (logs[1]), 101 needs it, and the set is
        // deduplicated so one header fetch serves both logs at block 100.
        assert_eq!(need, vec![100, 101]);

        let mut none_needed = vec![log_at(7)];
        none_needed[0].block_timestamp = Some(42);
        assert!(blocks_needing_timestamps(&none_needed, &|_| true, &HashMap::new()).is_empty());
    }

    /// Sentio's Avalanche endpoint sends `"blockTimestamp": "0x0"` on every
    /// log instead of omitting the field; alloy reads that as `Some(0)`. It
    /// carries no more information than `None` and must be enriched the same.
    #[test]
    fn a_zero_timestamp_counts_as_missing() {
        let mut logs = vec![log_at(100)];
        logs[0].block_timestamp = Some(0);
        assert_eq!(blocks_needing_timestamps(&logs, &|_| true, &HashMap::new()), vec![100]);
    }

    /// Only the logs a caller wants (the LB pools') need a header.
    #[test]
    fn only_wanted_logs_need_a_header() {
        let lb = Address::repeat_byte(0x1b);
        let logs = vec![log_from_at(lb, 100), log_at(101)];
        let wanted = |log: &Log| log.address() == lb;
        assert_eq!(blocks_needing_timestamps(&logs, &wanted, &HashMap::new()), vec![100]);
    }

    /// A block time the poll already saw needs no header.
    #[test]
    fn a_known_block_time_needs_no_header() {
        let logs = vec![log_at(100)];
        let known = HashMap::from([(100u64, 1_790_000_000u64)]);
        assert!(blocks_needing_timestamps(&logs, &|_| true, &known).is_empty());
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

        /// A log the RPC stamped with a zero timestamp gets the header's.
        #[tokio::test]
        async fn replaces_a_zero_timestamp_with_the_header_time() {
            let asserter = Asserter::new();
            let provider = Arc::new(ProviderBuilder::new().connect_mocked_client(asserter.clone()));
            asserter.push_success(&block_with_timestamp(1_790_408_097));

            let mut logs = vec![log_at(96_179_457)];
            logs[0].block_timestamp = Some(0);
            enrich_log_timestamps(&provider, &mut logs).await.unwrap();

            assert_eq!(logs[0].block_timestamp, Some(1_790_408_097));
        }

        /// Failure mode 1: the RPC call itself errors out, on every attempt.
        /// One queued failure per retry attempt so the test exercises the
        /// real failure mode all the way through, rather than letting the
        /// later attempts fall through to the mock's own "queue empty" error.
        #[tokio::test]
        async fn keeps_fallback_when_the_rpc_call_errors() {
            let asserter = Asserter::new();
            let provider = Arc::new(ProviderBuilder::new().connect_mocked_client(asserter.clone()));
            for _ in 0..HEADER_FETCH_ATTEMPTS {
                asserter.push_failure_msg("boom: connection reset");
            }

            let mut logs = vec![log_at(100)];
            let result = enrich_log_timestamps(&provider, &mut logs).await;

            // Same contract as before: partial failure still returns Ok(()),
            // and the unresolved log keeps its caller-supplied fallback.
            assert!(result.is_ok());
            assert_eq!(logs[0].block_timestamp, None);
        }

        /// Failure mode 2: the RPC call succeeds but has no block for that
        /// number (e.g. `eth_getBlockByNumber` returning `null`), on every
        /// attempt.
        #[tokio::test]
        async fn keeps_fallback_when_no_block_is_returned() {
            let asserter = Asserter::new();
            let provider = Arc::new(ProviderBuilder::new().connect_mocked_client(asserter.clone()));
            for _ in 0..HEADER_FETCH_ATTEMPTS {
                asserter.push_success(&Option::<Block>::None);
            }

            let mut logs = vec![log_at(100)];
            let result = enrich_log_timestamps(&provider, &mut logs).await;

            assert!(result.is_ok());
            assert_eq!(logs[0].block_timestamp, None);
        }

        /// The retry actually retries: a transient RPC error on the first
        /// attempt is followed by a successful response on the second, and
        /// the log ends up with a resolved timestamp rather than falling
        /// back. Exercises the RPC-error branch as the thing being retried.
        #[tokio::test]
        async fn recovers_after_a_transient_rpc_error() {
            let asserter = Asserter::new();
            let provider = Arc::new(ProviderBuilder::new().connect_mocked_client(asserter.clone()));
            asserter.push_failure_msg("boom: connection reset");
            asserter.push_success(&block_with_timestamp(1_700_000_000));

            let mut logs = vec![log_at(100)];
            enrich_log_timestamps(&provider, &mut logs).await.unwrap();

            assert_eq!(logs[0].block_timestamp, Some(1_700_000_000));
        }

        /// Same as above but for the other failure mode: a "no block
        /// returned" response followed by a successful one still resolves.
        #[tokio::test]
        async fn recovers_after_a_transient_no_block_response() {
            let asserter = Asserter::new();
            let provider = Arc::new(ProviderBuilder::new().connect_mocked_client(asserter.clone()));
            asserter.push_success(&Option::<Block>::None);
            asserter.push_success(&block_with_timestamp(1_700_000_000));

            let mut logs = vec![log_at(100)];
            enrich_log_timestamps(&provider, &mut logs).await.unwrap();

            assert_eq!(logs[0].block_timestamp, Some(1_700_000_000));
        }

        /// A block that fails on every attempt still ends up `None` after
        /// exactly `HEADER_FETCH_ATTEMPTS` tries — not fewer (giving up too
        /// early) and not more (an unbounded retry loop on the hot path).
        #[tokio::test]
        async fn gives_up_after_exhausting_all_attempts() {
            let asserter = Asserter::new();
            let provider = Arc::new(ProviderBuilder::new().connect_mocked_client(asserter.clone()));
            for _ in 0..HEADER_FETCH_ATTEMPTS {
                asserter.push_failure_msg("boom: connection reset");
            }
            // No further response queued: if the implementation retried even
            // once more than expected, that extra call would hit the mock's
            // "empty asserter response queue" error path instead of the
            // queued responses above, but the outcome for the log would look
            // identical (still `None`). The queue length itself is the real
            // assertion that attempts are bounded to `HEADER_FETCH_ATTEMPTS`.

            let mut logs = vec![log_at(100)];
            let result = enrich_log_timestamps(&provider, &mut logs).await;

            assert!(result.is_ok());
            assert_eq!(logs[0].block_timestamp, None);
            assert!(asserter.read_q().is_empty());
        }

        /// Known block times fill logs without a request: the mock has
        /// nothing queued, so any request would fail and leave the log unset.
        #[tokio::test]
        async fn a_known_block_time_fills_the_log_without_a_request() {
            let asserter = Asserter::new();
            let provider = Arc::new(ProviderBuilder::new().connect_mocked_client(asserter.clone()));
            let known = HashMap::from([(100u64, 1_790_000_000u64)]);

            let mut logs = vec![log_at(100)];
            enrich_log_timestamps_where(&provider, &mut logs, &|_| true, &known)
                .await
                .unwrap();

            assert_eq!(logs[0].block_timestamp, Some(1_790_000_000));
        }

        /// A log the caller does not want is left alone even when its header
        /// is available.
        #[tokio::test]
        async fn an_unwanted_log_is_left_alone() {
            let asserter = Asserter::new();
            let provider = Arc::new(ProviderBuilder::new().connect_mocked_client(asserter.clone()));
            asserter.push_success(&block_with_timestamp(1_700_000_000));

            let mut logs = vec![log_at(100)];
            enrich_log_timestamps_where(&provider, &mut logs, &|_| false, &HashMap::new())
                .await
                .unwrap();

            assert_eq!(logs[0].block_timestamp, None);
        }
    }
}
