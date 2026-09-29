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
//! The full pass also calls [`pools_needing_reads`], which rescans the whole
//! registry rather than trusting the tracked set. A reconfiguration applied
//! outside the updater's batch loop -- `catchup_registry_to_block` (used by
//! `CollectorHandle::add_pools`/`add_pools_ws`) and
//! `WebsocketBlockSource::bootstrap` both call `apply_log` on registry pools
//! directly -- resets a pool's `FeeSource` without going through
//! [`fee_read_candidates`], so without this rescan it would never be tracked
//! again.
//!
//! The updater never waits on a read: [`FeeReader::after_batch`] spawns it. At
//! most one read runs at a time; pools touched meanwhile wait for the next.
//! The spawned task is bounded by [`READ_TIMEOUT`] and clears `busy` on any
//! exit -- normal, error, timeout, or a panic unwinding -- so a hung or
//! failing read cannot leave the reader permanently idle. This replaces a
//! refetch of every Algebra pool that the updater awaited after each batch --
//! 30,659 multicalls in run 8's 12 hours on Avalanche, in which not one fee
//! changed.
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
use log::{debug, info, warn};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often every tracked pool is read, touched or not.
pub const FULL_READ_INTERVAL: Duration = Duration::from_secs(30);

/// A read that hangs -- a dead RPC endpoint, a multicall that never resolves
/// -- must not leave the reader `busy` forever: it would silently stop
/// picking up new candidates until the process restarts.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of calls bundled into one multicall.
const CHUNK_SIZE: usize = 250;

/// Attempts at one read, and the pause between them. A read is made at the
/// block the collector just processed, which the RPC node may not have
/// accepted yet -- "block not found: not accepted yet" failed 147 of run 11's
/// 965 reads (Avalanche, websocket mode, 2026-09-29). A node is usually a
/// moment behind, so the read is repeated at the same block rather than
/// dropped until the next pass.
const READ_ATTEMPTS: u32 = 3;
const READ_RETRY_DELAY: Duration = Duration::from_millis(50);

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

/// Every currently-registered pool whose [`FeeSource`] needs a read,
/// re-tracking each one this finds.
///
/// A reconfiguration applied outside the updater's batch loop --
/// `catchup_registry_to_block` (`src/collector/handle.rs`, used by
/// `CollectorHandle::add_pools`/`add_pools_ws`) and
/// `WebsocketBlockSource::bootstrap` (`src/collector/block_source.rs`) both
/// call `apply_log` on registry pools directly -- resets a pool's
/// `FeeSource` to `Unknown` without going through [`fee_read_candidates`],
/// so it gets neither a full pass nor a touch-triggered read and is priced
/// at its old fee indefinitely. The periodic full pass calls this instead of
/// trusting the tracked set, which is how such a pool is found again.
pub async fn pools_needing_reads(registry: &PoolRegistry) -> Vec<Address> {
    let mut addresses = Vec::new();
    for pool in registry.get_all_pools() {
        let guard = pool.read().await;
        let Some(v3) = guard.as_any().downcast_ref::<UniswapV3Pool>() else {
            continue;
        };
        if v3.fee_needs_reading() {
            addresses.push(guard.address());
        }
    }
    for &address in &addresses {
        registry.add_dynamic_fee_address(address);
    }
    addresses
}

/// What [`FeeReadSchedule::next_read`] found due: which addresses, and
/// whether this is the periodic full pass rather than a touch-triggered read.
#[derive(Debug)]
pub struct FeeRead {
    pub addresses: Vec<Address>,
    pub full_pass: bool,
}

/// When to read which pools. Pure bookkeeping, tested without a chain.
#[derive(Debug, Default)]
pub struct FeeReadSchedule {
    queued: HashSet<Address>,
    last_full_read: Option<Instant>,
}

impl FeeReadSchedule {
    /// Queue `touched`. Unless `busy`, return everything queued -- plus every
    /// `tracked` pool when a full pass is due. A due pass always returns
    /// `Some`, even with nothing to read: it must still run on schedule when
    /// nothing is tracked yet, which is exactly when [`pools_needing_reads`]
    /// (called by the due full pass) is what finds a pool a bypassed path
    /// reset. Otherwise, `None` when nothing is queued.
    pub fn next_read(
        &mut self,
        touched: impl IntoIterator<Item = Address>,
        tracked: impl FnOnce() -> Vec<Address>,
        now: Instant,
        busy: bool,
    ) -> Option<FeeRead> {
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
        } else if self.queued.is_empty() {
            return None;
        }
        Some(FeeRead {
            addresses: self.queued.drain().collect(),
            full_pass: full_due,
        })
    }
}

/// Clears `busy` on drop: a normal return, an early `?` inside `read_fees`, a
/// timeout, and a panic unwinding through the spawned task all release it the
/// same way. Before this a busy flag left set by anything other than the
/// happy path could strand the reader idle for the rest of the process.
struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
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
        let Some(FeeRead {
            mut addresses,
            full_pass,
        }) = self.schedule.next_read(
            touched,
            move || tracked_source.get_dynamic_fee_addresses(),
            Instant::now(),
            busy,
        )
        else {
            return;
        };
        self.busy.store(true, Ordering::Release);
        let provider = Arc::clone(&self.provider);
        let registry = Arc::clone(&self.registry);
        let busy = Arc::clone(&self.busy);
        let (multicall_address, chain_id) = (self.multicall_address, self.chain_id);
        tokio::spawn(async move {
            let _guard = BusyGuard(busy);
            // Kept in case the timeout below fires before the full-pass
            // rescan (if any), inside it, settles on a final address list --
            // that future is dropped on timeout, taking its own count with it.
            let queued_count = addresses.len();
            // The rescan and the read share one timeout: a rescan can only
            // block on a pool's own lock (never the network), but nothing
            // should be able to leave the reader `busy` past `READ_TIMEOUT`.
            let outcome = tokio::time::timeout(READ_TIMEOUT, async move {
                if full_pass {
                    // A reconfiguration applied outside the updater loop
                    // never reaches `fee_read_candidates` (see
                    // `pools_needing_reads`), so the full pass rescans the
                    // registry directly rather than trusting the tracked set
                    // alone.
                    addresses.extend(pools_needing_reads(&registry).await);
                    addresses.sort();
                    addresses.dedup();
                }
                let result = read_fees(
                    &provider,
                    &registry,
                    &addresses,
                    multicall_address,
                    chain_id,
                )
                .await;
                (addresses.len(), result)
            })
            .await;
            match outcome {
                Ok((_, Ok(()))) => {}
                Ok((count, Err(e))) => warn!(
                    "[Chain {}] Fee reader: read of {} pool(s) failed: {}",
                    chain_id, count, e
                ),
                Err(_) => warn!(
                    "[Chain {}] Fee reader: read of {} pool(s) timed out after {}s",
                    chain_id,
                    queued_count,
                    READ_TIMEOUT.as_secs()
                ),
            }
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

/// What `read_fees` must ask the chain for, sorted by what each address'
/// current classification needs.
struct ReadPlan {
    want_fee: Vec<Address>,
    want_config: Vec<Address>,
    want_current_fee: Vec<Address>,
}

/// Plan the reads `addresses` need from their current classification.
///
/// `(Events, AlgebraV3)` is planned like `Unknown`, not skipped. It only
/// reaches here already queued -- touched while still tracked, or
/// reconfigured (see [`fee_read_candidates`]) -- and a stale write-back from
/// an in-flight read that raced a reconfiguration can leave such a pool
/// classified `Events` while `DYNAMIC_FEE` is actually back on: `read_fees`
/// plans before its RPC call and writes back after, with nothing to notice a
/// reset landing in between. Re-deriving the classification here costs at
/// most one extra `fee()`/`globalState()` call for a pool that ever arrives
/// this way.
async fn plan_reads(registry: &PoolRegistry, addresses: &[Address]) -> ReadPlan {
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
            (FeeSource::Unknown, V3PoolType::AlgebraV3)
            | (FeeSource::Events, V3PoolType::AlgebraV3) => {
                want_fee.push(address);
                want_config.push(address);
            }
            _ => {}
        }
    }
    ReadPlan {
        want_fee,
        want_config,
        want_current_fee,
    }
}

/// Read what each pool's [`FeeSource`] needs at the last processed block and
/// write it back: the fee, a source for `Unknown` (and re-queued `Events`,
/// see [`plan_reads`]) pools, and no more tracking for pools that turn out to
/// be event-driven.
pub async fn read_fees<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    registry: &Arc<PoolRegistry>,
    addresses: &[Address],
    multicall_address: Address,
    chain_id: u64,
) -> Result<()> {
    let ReadPlan {
        want_fee,
        want_config,
        want_current_fee,
    } = plan_reads(registry, addresses).await;
    if want_fee.is_empty() && want_current_fee.is_empty() {
        return Ok(());
    }

    let block = BlockId::Number(BlockNumberOrTag::Number(
        registry.get_last_processed_block(),
    ));
    let mut attempt = 1;
    let (fees, configs, current_fees) = loop {
        match tokio::try_join!(
            read_fee_calls(provider, &want_fee, multicall_address, block),
            read_plugin_configs(provider, &want_config, multicall_address, block),
            read_current_fees(provider, &want_current_fee, multicall_address, block),
        ) {
            Ok(read) => break read,
            Err(e) if attempt < READ_ATTEMPTS => {
                debug!(
                    "[Chain {}] Fee reader: read at {} failed (attempt {}), retrying in {}ms: {}",
                    chain_id,
                    block,
                    attempt,
                    READ_RETRY_DELAY.as_millis(),
                    e
                );
                attempt += 1;
                tokio::time::sleep(READ_RETRY_DELAY).await;
            }
            Err(e) => return Err(e),
        }
    };

    let (mut changed, mut classified) = (0usize, 0usize);
    for (i, address) in want_fee.iter().enumerate() {
        let Some(fee) = fees[i] else {
            warn!(
                "[Chain {}] Fee reader: fee() failed for {}",
                chain_id, address
            );
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
                // Tracking follows the classification just derived, not
                // whatever it was before: a pool that arrived here as a
                // stale `Events` (see `plan_reads`) must be re-tracked if it
                // turns out `ReadFee` after all.
                match source {
                    FeeSource::ReadFee => registry.add_dynamic_fee_address(*address),
                    FeeSource::Events => registry.remove_dynamic_fee_address(address),
                    _ => {}
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
                    (fee_changed, was_unknown, false, false)
                }
                // A revert means the function is not there: an ordinary
                // Ramses-family pool whose FeeAdjustment events keep it current.
                None if was_unknown => {
                    v3.fee_source = FeeSource::Events;
                    (false, true, true, false)
                }
                // Already classified `ReadCurrentFee`: a transient failure,
                // not a reclassification -- one revert should not give up a
                // source that has been answering.
                None => (false, false, false, true),
            }
        })
        .await;
        if let Some((fee_changed, newly_classified, now_events, read_failed)) = outcome {
            changed += usize::from(fee_changed);
            classified += usize::from(newly_classified);
            if now_events {
                registry.remove_dynamic_fee_address(address);
            }
            if read_failed {
                warn!(
                    "[Chain {}] Fee reader: currentFee() failed for {}",
                    chain_id, address
                );
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
    use crate::PoolInterface;
    use alloy::primitives::{address, LogData, B256, U160};

    const A: Address = address!("0x00000000000000000000000000000000000000aa");
    const B: Address = address!("0x00000000000000000000000000000000000000bb");
    const C: Address = address!("0x00000000000000000000000000000000000000cc");
    const D: Address = address!("0x00000000000000000000000000000000000000dd");

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
        let got = schedule
            .next_read([], || vec![A, B], Instant::now(), false)
            .unwrap();
        assert_eq!(sorted(got.addresses), vec![A, B]);
        assert!(got.full_pass);
    }

    #[test]
    fn between_full_passes_only_touched_pools_are_read() {
        let mut schedule = FeeReadSchedule::default();
        let start = Instant::now();
        schedule.next_read([], || vec![A, B], start, false);
        let later = start + Duration::from_secs(5);
        let got = schedule.next_read([C], not_due, later, false).unwrap();
        assert_eq!(got.addresses, vec![C]);
        assert!(!got.full_pass);
        assert!(schedule.next_read([], not_due, later, false).is_none());
    }

    #[test]
    fn a_full_pass_comes_due_after_the_interval() {
        let mut schedule = FeeReadSchedule::default();
        let start = Instant::now();
        schedule.next_read([], || vec![A], start, false);
        let got = schedule
            .next_read([], || vec![A, B], start + FULL_READ_INTERVAL, false)
            .unwrap();
        assert_eq!(sorted(got.addresses), vec![A, B]);
        assert!(got.full_pass);
    }

    /// A due full pass on a registry tracking (and queuing) nothing must
    /// still run: it is the only way a pool a bypassed path reset -- itself
    /// untracked, so never queued -- is ever found again (`pools_needing_reads`
    /// runs off the `full_pass` flag this carries, not off a non-empty list).
    #[test]
    fn a_due_full_pass_runs_even_with_nothing_tracked_or_queued() {
        let mut schedule = FeeReadSchedule::default();
        let got = schedule
            .next_read([], Vec::new, Instant::now(), false)
            .unwrap();
        assert_eq!(got.addresses, Vec::<Address>::new());
        assert!(got.full_pass);
    }

    #[test]
    fn touched_pools_wait_while_a_read_is_running() {
        let mut schedule = FeeReadSchedule::default();
        let start = Instant::now();
        // The first call is a due full pass with nothing tracked or queued --
        // see `a_due_full_pass_runs_even_with_nothing_tracked_or_queued` --
        // which must return `Some` here too, not be skipped as a no-op.
        let first = schedule.next_read([], Vec::new, start, false).unwrap();
        assert!(first.full_pass);
        assert!(schedule.next_read([C], Vec::new, start, true).is_none());
        let got = schedule.next_read([], Vec::new, start, false).unwrap();
        assert_eq!(got.addresses, vec![C]);
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
            log_from(D, IAlgebraIntegralPool::Plugin::SIGNATURE_HASH),
        ];
        let got = fee_read_candidates(&events, &registry);
        assert_eq!(got, HashSet::from([A, C, D]));
        for reconfigured in [C, D] {
            assert!(
                registry.is_dynamic_fee_address(&reconfigured),
                "a reconfigured pool is tracked from now on"
            );
        }
    }

    /// Build a V3 pool of a given type and classification. Only the address,
    /// pool type and fee source matter for these tests.
    fn v3_pool(
        address: Address,
        pool_type: V3PoolType,
        fee_source: FeeSource,
    ) -> Box<dyn PoolInterface + Send + Sync> {
        let mut pool = UniswapV3Pool::new(
            address,
            Address::ZERO,
            Address::ZERO,
            U24::from(3000u32),
            60,
            U160::ZERO,
            0,
            0,
            Address::ZERO,
            pool_type,
        );
        pool.fee_source = fee_source;
        Box::new(pool)
    }

    #[tokio::test]
    async fn plan_reads_sorts_each_pool_by_what_its_fee_source_needs() {
        let registry = PoolRegistry::new(1);
        let events_algebra = address!("0x00000000000000000000000000000000000001e1");
        let read_fee_algebra = address!("0x00000000000000000000000000000000000001e2");
        let unknown_ramses = address!("0x00000000000000000000000000000000000001e3");
        let events_uniswap = address!("0x00000000000000000000000000000000000001e4");
        registry.add_pool(v3_pool(
            events_algebra,
            V3PoolType::AlgebraV3,
            FeeSource::Events,
        ));
        registry.add_pool(v3_pool(
            read_fee_algebra,
            V3PoolType::AlgebraV3,
            FeeSource::ReadFee,
        ));
        registry.add_pool(v3_pool(
            unknown_ramses,
            V3PoolType::RamsesCL,
            FeeSource::Unknown,
        ));
        registry.add_pool(v3_pool(
            events_uniswap,
            V3PoolType::UniswapV3,
            FeeSource::Events,
        ));

        let addresses = [
            events_algebra,
            read_fee_algebra,
            unknown_ramses,
            events_uniswap,
        ];
        let plan = plan_reads(&registry, &addresses).await;

        assert_eq!(
            plan.want_fee,
            vec![events_algebra, read_fee_algebra],
            "Events+AlgebraV3 is re-checked like Unknown; ReadFee always needs its fee"
        );
        assert_eq!(
            plan.want_config,
            vec![events_algebra],
            "only the unclassified/re-checked Algebra pool needs globalState()"
        );
        assert_eq!(plan.want_current_fee, vec![unknown_ramses]);
    }

    /// A reconfiguration applied outside the updater loop (`catchup_registry_to_block`,
    /// `WebsocketBlockSource::bootstrap`) resets `fee_source` by writing the
    /// pool directly, the same way `apply_log` always has, without going
    /// through `fee_read_candidates`. The periodic full pass must still find
    /// such a pool and re-track it.
    #[tokio::test]
    async fn pools_needing_reads_rescans_a_pool_reset_outside_the_updater() {
        let registry = PoolRegistry::new(1);
        let algebra = address!("0x00000000000000000000000000000000000002e1");
        let uniswap = address!("0x00000000000000000000000000000000000002e2");
        registry.add_pool(v3_pool(algebra, V3PoolType::AlgebraV3, FeeSource::Events));
        registry.add_pool(v3_pool(uniswap, V3PoolType::UniswapV3, FeeSource::Events));
        assert!(
            !registry.is_dynamic_fee_address(&algebra),
            "starts untracked, like any classified Events pool"
        );

        let pool = registry.get_pool(&algebra).unwrap();
        {
            let mut guard = pool.write().await;
            let v3 = guard.as_any_mut().downcast_mut::<UniswapV3Pool>().unwrap();
            v3.fee_source = FeeSource::Unknown;
        }

        let found = pools_needing_reads(&registry).await;
        assert_eq!(found, vec![algebra]);
        assert!(
            registry.is_dynamic_fee_address(&algebra),
            "rescanning must re-track a pool a bypassed path reset"
        );
        assert!(!registry.is_dynamic_fee_address(&uniswap));
    }

    #[test]
    fn dropping_the_busy_guard_clears_the_flag() {
        let busy = Arc::new(AtomicBool::new(true));
        {
            let _guard = BusyGuard(Arc::clone(&busy));
            assert!(busy.load(Ordering::Acquire));
        }
        assert!(!busy.load(Ordering::Acquire));
    }
}
