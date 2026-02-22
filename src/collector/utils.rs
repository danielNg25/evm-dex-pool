use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, FixedBytes};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use anyhow::Result;
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
