use alloy::primitives::Address;
use alloy::providers::Provider;
use anyhow::Result;
use std::future::Future;
use std::sync::Arc;

/// Trait for resolving token address to (address, decimals).
/// Implementors should cache results and fetch from RPC if not cached.
pub trait TokenInfo: Send + Sync {
    fn get_or_fetch_token<P: Provider + Send + Sync>(
        &self,
        provider: &Arc<P>,
        address: Address,
        multicall_address: Address,
    ) -> impl Future<Output = Result<(Address, u8)>> + Send;
}
