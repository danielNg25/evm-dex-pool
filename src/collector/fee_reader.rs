//! Background reads of the V3 fees no event announces.
//!
//! Most pools keep their fee current from events -- Uniswap V3 never changes
//! it, Ramses-family pools emit `FeeAdjustment`, Algebra pools with
//! `DYNAMIC_FEE` off emit `Fee(uint16)`. The rest are read here, off the
//! collector's hot path: [`FeeSource::ReadFee`] (Algebra with `DYNAMIC_FEE`
//! on: the plugin computes `fee()`), [`FeeSource::ReadCurrentFee`] (a
//! Ramses-family pool that answers `currentFee()`), and pools still
//! [`FeeSource::Unknown`] (restored from a snapshot, or reconfigured on
//! chain), which one read classifies.
//!
//! When: after a batch, the tracked pools that had any event in it (a swap
//! feeds the volatility oracle an adaptive fee reads; a plugin change can
//! switch the source), and every [`FULL_READ_INTERVAL`], every tracked pool,
//! because an adaptive fee also drifts with time alone -- Flare 0x9af6 went
//! 200 -> 224 over ~80 minutes with no swap. On Flare, `fee()` read one block
//! before a swap matched what that swap paid 390 times in 391.
//!
//! The updater never waits on a read: [`FeeReader::after_batch`] spawns it. At
//! most one read runs at a time; pools touched meanwhile wait for the next.
//! This replaces a refetch of every Algebra pool that the updater awaited
//! after each batch -- 30,659 multicalls in run 8's 12 hours on Avalanche, in
//! which not one fee changed.
//!
//! `fee()` is read through the Uniswap V3 ABI, which types it `uint24`.
//! Algebra declares it `uint16`; both ABI-encode to a 32-byte word, so the
//! wider type decodes an Algebra response correctly.

use crate::contracts::IAlgebraIntegralPool;
use crate::contracts_rpc::{
    RpcAlgebraV3Pool as AlgebraV3Pool, RpcIUniswapV3Pool as IUniswapV3Pool,
    RpcRamsesCLPool as RamsesCLPool,
};
use crate::v3::{FeeSource, UniswapV3Pool, V3PoolType, ALGEBRA_DYNAMIC_FEE_FLAG};
use crate::PoolRegistry;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{aliases::U24, Address};
use alloy::providers::{MulticallBuilder, Provider};
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;
use anyhow::Result;
use log::{info, warn};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often every tracked pool is read, touched or not.
pub const FULL_READ_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum number of calls bundled into one multicall.
const CHUNK_SIZE: usize = 250;

/// The tracked pools a batch touched, plus any pool whose plugin changed.
/// A reconfigured pool is tracked from here on even if its fee was
/// event-driven until now: the change may have made it a read.
pub fn fee_read_candidates(events: &[Log], registry: &PoolRegistry) -> HashSet<Address> {
    let mut candidates = HashSet::new();
    for event in events {
        let address = event.address();
        let reconfigured = matches!(
            event.topic0(),
            Some(topic) if *topic == IAlgebraIntegralPool::PluginConfig::SIGNATURE_HASH
                || *topic == IAlgebraIntegralPool::Plugin::SIGNATURE_HASH
        );
        if reconfigured {
            registry.add_dynamic_fee_address(address);
        }
        if reconfigured || registry.is_dynamic_fee_address(&address) {
            candidates.insert(address);
        }
    }
    candidates
}

/// When to read which pools. Pure bookkeeping, tested without a chain.
#[derive(Debug, Default)]
pub struct FeeReadSchedule {
    queued: HashSet<Address>,
    last_full_read: Option<Instant>,
}

impl FeeReadSchedule {
    /// Queue `touched`. Unless `busy`, return everything queued -- plus every
    /// `tracked` pool when a full pass is due -- or `None` if that is nothing.
    pub fn next_read(
        &mut self,
        touched: impl IntoIterator<Item = Address>,
        tracked: impl FnOnce() -> Vec<Address>,
        now: Instant,
        busy: bool,
    ) -> Option<Vec<Address>> {
        self.queued.extend(touched);
        if busy {
            return None;
        }
        let full_due = self
            .last_full_read
            .is_none_or(|at| now.duration_since(at) >= FULL_READ_INTERVAL);
        if full_due {
            self.queued.extend(tracked());
            self.last_full_read = Some(now);
        }
        if self.queued.is_empty() {
            return None;
        }
        Some(self.queued.drain().collect())
    }
}

/// Owns the schedule and spawns the reads.
pub struct FeeReader<P: Provider + Send + Sync + 'static> {
    provider: Arc<P>,
    registry: Arc<PoolRegistry>,
    multicall_address: Address,
    chain_id: u64,
    schedule: FeeReadSchedule,
    busy: Arc<AtomicBool>,
}

impl<P: Provider + Send + Sync + 'static> FeeReader<P> {
    pub fn new(provider: Arc<P>, registry: Arc<PoolRegistry>, multicall_address: Address) -> Self {
        let chain_id = registry.get_network_id();
        Self {
            provider,
            registry,
            multicall_address,
            chain_id,
            schedule: FeeReadSchedule::default(),
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Call after every batch with [`fee_read_candidates`]. Never waits.
    pub fn after_batch(&mut self, touched: HashSet<Address>) {
        let busy = self.busy.load(Ordering::Acquire);
        let tracked_source = Arc::clone(&self.registry);
        let Some(addresses) = self.schedule.next_read(
            touched,
            move || tracked_source.get_dynamic_fee_addresses(),
            Instant::now(),
            busy,
        ) else {
            return;
        };
        self.busy.store(true, Ordering::Release);
        let provider = Arc::clone(&self.provider);
        let registry = Arc::clone(&self.registry);
        let busy = Arc::clone(&self.busy);
        let (multicall_address, chain_id) = (self.multicall_address, self.chain_id);
        tokio::spawn(async move {
            if let Err(e) = read_fees(&provider, &registry, &addresses, multicall_address, chain_id).await {
                warn!(
                    "[Chain {}] Fee reader: read of {} pool(s) failed: {}",
                    chain_id,
                    addresses.len(),
                    e
                );
            }
            busy.store(false, Ordering::Release);
        });
    }
}

/// Run `f` on the pool at `address` if it is a V3 pool.
async fn with_v3<R>(
    registry: &PoolRegistry,
    address: Address,
    f: impl FnOnce(&mut UniswapV3Pool) -> R,
) -> Option<R> {
    let pool = registry.get_pool(&address)?;
    let mut guard = pool.write().await;
    guard.as_any_mut().downcast_mut::<UniswapV3Pool>().map(f)
}

/// Read what each pool's [`FeeSource`] needs at the last processed block and
/// write it back: the fee, a source for `Unknown` pools, and no more tracking
/// for pools that turn out to be event-driven.
pub async fn read_fees<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    registry: &Arc<PoolRegistry>,
    addresses: &[Address],
    multicall_address: Address,
    chain_id: u64,
) -> Result<()> {
    // `fee()` gives the right value whether or not the plugin computes it, so
    // an unclassified Algebra pool needs it and `globalState()` for the flag.
    let mut want_fee = Vec::new();
    let mut want_config = Vec::new();
    let mut want_current_fee = Vec::new();
    for &address in addresses {
        let Some(pool) = registry.get_pool(&address) else {
            continue;
        };
        let guard = pool.read().await;
        let Some(v3) = guard.as_any().downcast_ref::<UniswapV3Pool>() else {
            continue;
        };
        match (v3.fee_source, v3.pool_type) {
            (FeeSource::ReadFee, _) => want_fee.push(address),
            (FeeSource::ReadCurrentFee, _) | (FeeSource::Unknown, V3PoolType::RamsesCL) => {
                want_current_fee.push(address)
            }
            (FeeSource::Unknown, V3PoolType::AlgebraV3) => {
                want_fee.push(address);
                want_config.push(address);
            }
            _ => {}
        }
    }
    if want_fee.is_empty() && want_current_fee.is_empty() {
        return Ok(());
    }

    let block = BlockId::Number(BlockNumberOrTag::Number(registry.get_last_processed_block()));
    let (fees, configs, current_fees) = tokio::try_join!(
        read_fee_calls(provider, &want_fee, multicall_address, block),
        read_plugin_configs(provider, &want_config, multicall_address, block),
        read_current_fees(provider, &want_current_fee, multicall_address, block),
    )?;

    let (mut changed, mut classified) = (0usize, 0usize);
    for (i, address) in want_fee.iter().enumerate() {
        let Some(fee) = fees[i] else {
            warn!("[Chain {}] Fee reader: fee() failed for {}", chain_id, address);
            continue;
        };
        let config = want_config
            .iter()
            .position(|a| a == address)
            .and_then(|j| configs[j]);
        let outcome = with_v3(registry, *address, |v3| {
            let fee_changed = v3.fee != fee;
            v3.set_fee(fee);
            if let Some(config) = config {
                v3.fee_source = if config & ALGEBRA_DYNAMIC_FEE_FLAG != 0 {
                    FeeSource::ReadFee
                } else {
                    FeeSource::Events
                };
            }
            (fee_changed, v3.fee_source)
        })
        .await;
        if let Some((fee_changed, source)) = outcome {
            changed += usize::from(fee_changed);
            if config.is_some() {
                classified += 1;
                if source == FeeSource::Events {
                    registry.remove_dynamic_fee_address(address);
                }
            }
        }
    }

    for (i, address) in want_current_fee.iter().enumerate() {
        let current = current_fees[i];
        let outcome = with_v3(registry, *address, |v3| {
            let was_unknown = v3.fee_source == FeeSource::Unknown;
            match current {
                Some(fee) => {
                    let fee_changed = v3.fee != fee;
                    v3.set_fee(fee);
                    v3.fee_source = FeeSource::ReadCurrentFee;
                    (fee_changed, was_unknown, false)
                }
                // A revert means the function is not there: an ordinary
                // Ramses-family pool whose FeeAdjustment events keep it current.
                None if was_unknown => {
                    v3.fee_source = FeeSource::Events;
                    (false, true, true)
                }
                None => (false, false, false),
            }
        })
        .await;
        if let Some((fee_changed, newly_classified, now_events)) = outcome {
            changed += usize::from(fee_changed);
            classified += usize::from(newly_classified);
            if now_events {
                registry.remove_dynamic_fee_address(address);
            }
        }
    }

    info!(
        "[Chain {}] Fee reader: read {} pool(s), {} fee(s) changed, {} classified",
        chain_id,
        want_fee.len() + want_current_fee.len(),
        changed,
        classified
    );
    Ok(())
}

async fn read_fee_calls<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    addresses: &[Address],
    multicall_address: Address,
    block: BlockId,
) -> Result<Vec<Option<U24>>> {
    let mut out = Vec::with_capacity(addresses.len());
    for chunk in addresses.chunks(CHUNK_SIZE) {
        let mut mc = MulticallBuilder::new_dynamic(provider)
            .address(multicall_address)
            .block(block);
        for &address in chunk {
            mc = mc.add_dynamic(IUniswapV3Pool::new(address, provider).fee());
        }
        out.extend(mc.try_aggregate(false).await?.into_iter().map(|r| r.ok()));
    }
    Ok(out)
}

async fn read_plugin_configs<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    addresses: &[Address],
    multicall_address: Address,
    block: BlockId,
) -> Result<Vec<Option<u8>>> {
    let mut out = Vec::with_capacity(addresses.len());
    for chunk in addresses.chunks(CHUNK_SIZE) {
        let mut mc = MulticallBuilder::new_dynamic(provider)
            .address(multicall_address)
            .block(block);
        for &address in chunk {
            mc = mc.add_dynamic(AlgebraV3Pool::new(address, provider).globalState());
        }
        out.extend(
            mc.try_aggregate(false)
                .await?
                .into_iter()
                .map(|r| r.ok().map(|state| state.pluginConfig)),
        );
    }
    Ok(out)
}

async fn read_current_fees<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    addresses: &[Address],
    multicall_address: Address,
    block: BlockId,
) -> Result<Vec<Option<U24>>> {
    let mut out = Vec::with_capacity(addresses.len());
    for chunk in addresses.chunks(CHUNK_SIZE) {
        let mut mc = MulticallBuilder::new_dynamic(provider)
            .address(multicall_address)
            .block(block);
        for &address in chunk {
            mc = mc.add_dynamic(RamsesCLPool::new(address, provider).currentFee());
        }
        out.extend(mc.try_aggregate(false).await?.into_iter().map(|r| r.ok()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, LogData, B256};

    const A: Address = address!("0x00000000000000000000000000000000000000aa");
    const B: Address = address!("0x00000000000000000000000000000000000000bb");
    const C: Address = address!("0x00000000000000000000000000000000000000cc");

    fn sorted(mut v: Vec<Address>) -> Vec<Address> {
        v.sort();
        v
    }

    fn not_due() -> Vec<Address> {
        panic!("no full pass is due")
    }

    #[test]
    fn the_first_batch_reads_every_tracked_pool() {
        let mut schedule = FeeReadSchedule::default();
        let got = schedule.next_read([], || vec![A, B], Instant::now(), false);
        assert_eq!(sorted(got.unwrap()), vec![A, B]);
    }

    #[test]
    fn between_full_passes_only_touched_pools_are_read() {
        let mut schedule = FeeReadSchedule::default();
        let start = Instant::now();
        schedule.next_read([], || vec![A, B], start, false);
        let later = start + Duration::from_secs(5);
        assert_eq!(schedule.next_read([C], not_due, later, false), Some(vec![C]));
        assert_eq!(schedule.next_read([], not_due, later, false), None);
    }

    #[test]
    fn a_full_pass_comes_due_after_the_interval() {
        let mut schedule = FeeReadSchedule::default();
        let start = Instant::now();
        schedule.next_read([], || vec![A], start, false);
        let got = schedule.next_read([], || vec![A, B], start + FULL_READ_INTERVAL, false);
        assert_eq!(sorted(got.unwrap()), vec![A, B]);
    }

    #[test]
    fn touched_pools_wait_while_a_read_is_running() {
        let mut schedule = FeeReadSchedule::default();
        let start = Instant::now();
        schedule.next_read([], Vec::new, start, false);
        assert_eq!(schedule.next_read([C], Vec::new, start, true), None);
        assert_eq!(schedule.next_read([], Vec::new, start, false), Some(vec![C]));
    }

    fn log_from(address: Address, topic: B256) -> Log {
        Log {
            inner: alloy::primitives::Log {
                address,
                data: LogData::new_unchecked(vec![topic], Default::default()),
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_batch_queues_its_tracked_pools_and_any_reconfigured_pool() {
        let registry = PoolRegistry::new(1);
        registry.add_dynamic_fee_address(A);
        let swap = B256::repeat_byte(1);
        let events = vec![
            log_from(A, swap),
            log_from(B, swap), // untracked: its fee is event-driven
            log_from(C, IAlgebraIntegralPool::PluginConfig::SIGNATURE_HASH),
        ];
        let got = fee_read_candidates(&events, &registry);
        assert_eq!(got, HashSet::from([A, C]));
        assert!(
            registry.is_dynamic_fee_address(&C),
            "a reconfigured pool is tracked from now on"
        );
    }
}
