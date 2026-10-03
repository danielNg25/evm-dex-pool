use crate::collector::block_source::enrich_if_lb_pools_present;
use crate::collector::event_processor::fetch_events_with_retry;
use crate::{PoolRegistry, Topic};
use alloy::eips::BlockNumberOrTag;
use alloy::providers::Provider;
use alloy::rpc::types::Log;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

/// Fetch, LB-enrich, and canonically order one confirmed block's logs for the
/// registry's pools. Used by the bot's replay driver.
pub async fn fetch_block_logs<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    registry: &PoolRegistry,
    topics: &[Topic],
    block: u64,
) -> Result<Vec<Log>> {
    let mut logs = fetch_events_with_retry(
        provider,
        registry.get_all_addresses(),
        topics.to_vec(),
        BlockNumberOrTag::Number(block),
        BlockNumberOrTag::Number(block),
        registry.get_network_id(),
    )
    .await?;
    enrich_if_lb_pools_present(provider, registry, &mut logs, &HashMap::new()).await?;
    logs.sort_by_key(|l| (l.block_number.unwrap_or(0), l.log_index.unwrap_or(0)));
    Ok(logs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::providers::mock::Asserter;
    use alloy::providers::{ProviderBuilder, RootProvider};

    fn log_at(block: u64, log_index: u64) -> Log {
        Log {
            block_number: Some(block),
            transaction_index: Some(0),
            log_index: Some(log_index),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn fetch_block_logs_returns_logs_in_canonical_order() {
        // The RPC answers log_index 2 before log_index 0 for block 100.
        let asserter = Asserter::new();
        let provider: Arc<RootProvider> =
            Arc::new(ProviderBuilder::default().connect_mocked_client(asserter.clone()));
        asserter.push_success(&vec![log_at(100, 2), log_at(100, 0)]); // eth_getLogs
        let registry = PoolRegistry::new(43114);

        let logs = fetch_block_logs(&provider, &registry, &[], 100)
            .await
            .unwrap();
        let idx: Vec<u64> = logs.iter().map(|l| l.log_index.unwrap()).collect();
        assert_eq!(idx, vec![0, 2]);
    }
}
