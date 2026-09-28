use crate::contracts::{
    IAlgebraIntegralPool, IAlgebraPoolSei, IPancakeV3Pool, IRamsesCLPool, IUniswapV3Pool,
};
use crate::pool::base::{EventApplicable, PoolInterface, PoolType, PoolTypeTrait, TopicList};
use alloy::primitives::FixedBytes;
use alloy::primitives::{aliases::U24, Address, Signed, U160, U256};
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;
use anyhow::{anyhow, Result};
use log::{debug, info, trace, warn};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
};

use super::{v3_swap, Tick, TickMap, TickSearch};

/// The Q64.96 precision used by Uniswap V3
pub const Q96_U128: u128 = 1 << 96;
pub const FEE_DENOMINATOR: u32 = 1000000;

/// How many of a pool's recent swaps the fee check keeps, and how many of them
/// may have paid more than the fee held before the pool stops quoting. On
/// Flare the oracle-fee pools miss on most user swaps; the adaptive-fee pools,
/// read one block ahead, missed 1 in 391.
const SWAP_FEE_WINDOW: usize = 10;
const SWAP_FEE_MISSES_TO_STOP: usize = 3;

/// Enum representing the type of V3 pool
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum V3PoolType {
    UniswapV3,
    PancakeV3,
    AlgebraV3,
    AlgebraTwoSideFee,
    AlgebraPoolFeeInState,
    /// Ramses-family concentrated-liquidity fork (Pharaoh, Shadow, Nile, Cleo,
    /// Ramses CL). Swap math is identical to [`V3PoolType::UniswapV3`]; what
    /// differs is a mutable `fee()`. That fee is kept current by the pool's own
    /// `FeeAdjustment` events, applied in `apply_log`, not by polling.
    ///
    /// New variants go at the end: `bincode` encodes enums by positional index,
    /// and consumers persist `UniswapV3Pool` with it.
    RamsesCL,
}

/// Where a V3 pool's swap fee comes from, which decides how it is kept current.
///
/// Runtime-only: the pool field holding it is `#[serde(skip)]`, so persisted
/// snapshots keep their encoding and a restored pool comes back `Unknown`
/// until the collector's fee reader classifies it with one read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FeeSource {
    /// Not classified yet: restored from a snapshot, or reconfigured on chain.
    #[default]
    Unknown,
    /// Fixed until an event changes it: Uniswap V3, PancakeV3, Ramses
    /// `FeeAdjustment`, Algebra with DYNAMIC_FEE off (`Fee(uint16)`). No reads.
    Events,
    /// Algebra with DYNAMIC_FEE on: the plugin computes `fee()` on demand and
    /// nothing announces a change, so it is read in the background. Flare's
    /// adaptive pools drift with time alone (0x9af6: 200 -> 224 in ~80 min).
    ReadFee,
    /// A Ramses-family pool that answers `currentFee()`: its swaps pay that,
    /// whatever `fee()` says (0x0021368B: 75 against 50).
    ReadCurrentFee,
}

/// Algebra Integral `globalState().pluginConfig` bit: `fee()` comes from the
/// plugin, not the stored `lastFee`.
pub const ALGEBRA_DYNAMIC_FEE_FLAG: u8 = 0x80;

/// A freshly fetched pool's fee source, from what the fetch already read.
pub fn classify_fee_source(
    pool_type: V3PoolType,
    algebra_plugin_config: Option<u8>,
    answers_current_fee: bool,
) -> FeeSource {
    match pool_type {
        V3PoolType::AlgebraV3 => match algebra_plugin_config {
            Some(config) if config & ALGEBRA_DYNAMIC_FEE_FLAG != 0 => FeeSource::ReadFee,
            Some(_) => FeeSource::Events,
            None => FeeSource::Unknown,
        },
        V3PoolType::RamsesCL if answers_current_fee => FeeSource::ReadCurrentFee,
        _ => FeeSource::Events,
    }
}

/// Struct containing V3 pool information including tick data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniswapV3Pool {
    /// Pool type
    pub pool_type: V3PoolType,
    /// Pool address
    pub address: Address,
    /// First token address in the pool
    pub token0: Address,
    /// Second token address in the pool
    pub token1: Address,
    /// Fee tier in the pool in basis points 1000000 = 100%
    pub fee: U24,
    /// Tick spacing for this pool
    pub tick_spacing: i32,
    /// Current sqrt price (sqrt(token1/token0)) * 2^96
    pub sqrt_price_x96: U160,
    /// Current tick
    pub tick: i32,
    /// Current liquidity
    pub liquidity: u128,
    /// Mapping of initialized ticks
    pub ticks: TickMap,
    /// Factory address
    pub factory: Address,
    /// Last update timestamp
    pub last_updated: u64,
    /// Creation timestamp or block
    pub created_at: u64,
    /// See [`FeeSource`]. Runtime-only: skipped by serde, and so by bincode.
    #[serde(skip)]
    pub fee_source: FeeSource,
    /// Whether each of the last `SWAP_FEE_WINDOW` swaps paid more than the
    /// fee held (from Algebra's `SwapFee`). Runtime-only.
    #[serde(skip)]
    pub swap_fee_misses: VecDeque<bool>,
}

impl UniswapV3Pool {
    /// Create a new V3 pool
    pub fn new(
        address: Address,
        token0: Address,
        token1: Address,
        fee: U24,
        tick_spacing: i32,
        sqrt_price_x96: U160,
        tick: i32,
        liquidity: u128,
        factory: Address,
        pool_type: V3PoolType,
    ) -> Self {
        let current_time = chrono::Utc::now().timestamp() as u64;
        Self {
            pool_type,
            address,
            token0,
            token1,
            fee,
            tick_spacing,
            sqrt_price_x96,
            tick,
            liquidity,
            ticks: BTreeMap::new(),
            last_updated: current_time,
            created_at: current_time,
            factory,
            fee_source: FeeSource::Unknown,
            swap_fee_misses: VecDeque::new(),
        }
    }

    pub fn set_fee(&mut self, fee: U24) {
        self.fee = fee;
    }

    /// Whether the collector's fee reader must fetch this pool's fee: its
    /// source is a read, or it is an unclassified pool of a type that may turn
    /// out to need one.
    pub fn fee_needs_reading(&self) -> bool {
        match self.fee_source {
            FeeSource::ReadFee | FeeSource::ReadCurrentFee => true,
            FeeSource::Unknown => {
                matches!(self.pool_type, V3PoolType::AlgebraV3 | V3PoolType::RamsesCL)
            }
            FeeSource::Events => false,
        }
    }

    /// True while too many recent swaps paid more than the fee held: the fee
    /// the bot would price with is not what the pool charges, so it must not
    /// quote. The simulator treats a quote error as "skip this cycle".
    pub fn fee_unpredictable(&self) -> bool {
        self.swap_fee_misses
            .iter()
            .filter(|&&missed| missed)
            .count()
            >= SWAP_FEE_MISSES_TO_STOP
    }

    fn record_swap_fee(&mut self, missed: bool) {
        let was_stopped = self.fee_unpredictable();
        self.swap_fee_misses.push_back(missed);
        if self.swap_fee_misses.len() > SWAP_FEE_WINDOW {
            self.swap_fee_misses.pop_front();
        }
        match (was_stopped, self.fee_unpredictable()) {
            (false, true) => warn!(
                "Pool {}: {} of its last {} swaps paid more than the fee held ({}); not quoting it until that clears",
                self.address,
                self.swap_fee_misses.iter().filter(|&&m| m).count(),
                self.swap_fee_misses.len(),
                self.fee
            ),
            (true, false) => info!("Pool {}: swap fees match again; quoting resumed", self.address),
            _ => {}
        }
    }

    /// Update pool state based on swap event
    pub fn update_state(&mut self, sqrt_price_x96: U160, tick: i32, liquidity: u128) -> Result<()> {
        if sqrt_price_x96 == U160::ZERO {
            return Err(anyhow!("Invalid sqrt_price_x96: zero"));
        }
        if tick < -887272 || tick > 887272 {
            return Err(anyhow!("Invalid tick: {} out of bounds", tick));
        }
        self.sqrt_price_x96 = sqrt_price_x96;
        self.tick = tick;
        self.liquidity = liquidity;
        self.last_updated = chrono::Utc::now().timestamp() as u64;
        Ok(())
    }

    /// Add or update a tick
    pub fn update_tick(
        &mut self,
        index: i32,
        liquidity_net: i128,
        liquidity_gross: u128,
    ) -> Result<()> {
        if liquidity_gross == 0 {
            self.ticks.remove(&index);
        } else {
            let tick = Tick {
                index,
                liquidity_net,
                liquidity_gross,
            };
            self.ticks.insert(index, tick);
        }
        Ok(())
    }

    /// Get the price of token1 in terms of token0 from sqrt_price_x96
    pub fn get_price_from_sqrt_price(&self) -> Result<f64> {
        let sqrt_price: f64 = self.sqrt_price_x96.to::<u128>() as f64 / Q96_U128 as f64;
        Ok(sqrt_price * sqrt_price)
    }

    /// How this pool's swap loop finds where a step ends. See [`TickSearch`].
    pub fn tick_search(&self) -> TickSearch {
        match self.pool_type {
            // Algebra V1/V1.9's `TickTable.nextTickInTheSameRow` stops at the
            // edge of a 256-compressed-tick row exactly as
            // `TickBitmap.nextInitializedTickWithinOneWord` stops at a word
            // edge, so these two step the same as Uniswap V3 and its forks.
            // Only Algebra Integral (`AlgebraV3`) walks a linked list of
            // initialized ticks and never stops at an edge.
            V3PoolType::UniswapV3
            | V3PoolType::PancakeV3
            | V3PoolType::RamsesCL
            | V3PoolType::AlgebraTwoSideFee
            | V3PoolType::AlgebraPoolFeeInState => TickSearch::WordBounded {
                tick_spacing: self.tick_spacing,
            },
            V3PoolType::AlgebraV3 => TickSearch::NextInitialized,
        }
    }

    /// Calculate the amount of token1 for a given amount of token0
    fn calculate_zero_for_one(&self, amount: U256, is_exact_input: bool) -> Result<U256> {
        let amount_specified = if is_exact_input {
            Signed::from_raw(amount)
        } else {
            Signed::from_raw(amount).saturating_neg()
        };
        let swap_state = v3_swap(
            self.fee,
            self.sqrt_price_x96,
            self.tick,
            self.liquidity,
            &self.ticks,
            self.tick_search(),
            true,
            amount_specified,
            None,
        )?;
        if !swap_state.amount_specified_remaining.is_zero() {
            return Err(anyhow!(
                "Amount specified remaining: {}",
                swap_state.amount_specified_remaining
            ));
        }
        Ok(swap_state.amount_calculated.abs().into_raw())
    }

    /// Calculate the amount of token0 for a given amount of token1 (exact input)
    fn calculate_one_for_zero(&self, amount: U256, is_exact_input: bool) -> Result<U256> {
        let amount_specified = if is_exact_input {
            Signed::from_raw(amount)
        } else {
            Signed::from_raw(amount).saturating_neg()
        };
        let swap_state = v3_swap(
            self.fee,
            self.sqrt_price_x96,
            self.tick,
            self.liquidity,
            &self.ticks,
            self.tick_search(),
            false,
            amount_specified,
            None,
        )?;
        if !swap_state.amount_specified_remaining.is_zero() {
            return Err(anyhow!(
                "Amount specified remaining: {}",
                swap_state.amount_specified_remaining
            ));
        }
        Ok(swap_state.amount_calculated.abs().into_raw())
    }

    /// Get the adjacent initialized ticks for a given tick
    pub fn get_adjacent_ticks(&self, tick: i32) -> (Option<&Tick>, Option<&Tick>) {
        let below = self.ticks.range(..tick).next_back().map(|(_, tick)| tick);
        let above = self.ticks.range(tick..).next().map(|(_, tick)| tick);
        (below, above)
    }

    /// Check if the pool has sufficient liquidity
    pub fn has_sufficient_liquidity(&self) -> bool {
        self.liquidity != 0 && !self.ticks.is_empty()
    }

    /// Calculate the amount out for a swap with the exact formula
    pub fn calculate_exact_input(&self, token_in: &Address, amount_in: U256) -> Result<U256> {
        if self.fee_unpredictable() {
            return Err(anyhow!(
                "pool {}: recent swaps paid more than its fee; not quoting",
                self.address
            ));
        }
        let result;
        if token_in == &self.token0 {
            result = self.calculate_zero_for_one(amount_in, true)?;
        } else if token_in == &self.token1 {
            result = self.calculate_one_for_zero(amount_in, true)?;
        } else {
            return Err(anyhow!("Token not in pool"));
        }
        Ok(result)
    }

    /// Calculate the amount out for a swap with the exact formula
    pub fn calculate_exact_output(&self, token_out: &Address, amount_in: U256) -> Result<U256> {
        if self.fee_unpredictable() {
            return Err(anyhow!(
                "pool {}: recent swaps paid more than its fee; not quoting",
                self.address
            ));
        }
        if token_out == &self.token0 {
            self.calculate_one_for_zero(amount_in, false)
        } else if token_out == &self.token1 {
            self.calculate_zero_for_one(amount_in, false)
        } else {
            Err(anyhow!("Token not in pool"))
        }
    }

    /// Apply a swap to the pool, updating the internal state
    fn apply_swap_internal(
        &mut self,
        token_in: &Address,
        _amount_in: U256,
        _amount_out: U256,
    ) -> Result<()> {
        self.last_updated = chrono::Utc::now().timestamp() as u64;

        if !self.contains_token(token_in) {
            return Err(anyhow!("Token not in pool"));
        }

        Ok(())
    }

    /// Convert a tick to its corresponding word index in the tick bitmap
    pub fn tick_to_word(&self, tick: i32) -> i32 {
        let compressed = tick / self.tick_spacing;
        let compressed = if tick < 0 && tick % self.tick_spacing != 0 {
            compressed - 1
        } else {
            compressed
        };
        compressed >> 8
    }

    /// Helper for applying mint events: Uniswap V3's, and Ramses V3's, which
    /// adds a position `index` but moves liquidity the same way.
    fn apply_mint_event(&mut self, tick_lower: i32, tick_upper: i32, amount: u128) -> Result<()> {
        if tick_lower >= tick_upper {
            return Err(anyhow!(
                "Invalid tick range: tick_lower {} >= tick_upper {}",
                tick_lower,
                tick_upper
            ));
        }

        // Update tick_lower
        if let Some(tick) = self.ticks.get_mut(&tick_lower) {
            tick.liquidity_net = tick.liquidity_net.saturating_add(amount as i128);
            tick.liquidity_gross = tick.liquidity_gross.saturating_add(amount);
        } else {
            self.update_tick(tick_lower, amount as i128, amount)?;
        }

        // Update tick_upper
        if let Some(tick) = self.ticks.get_mut(&tick_upper) {
            tick.liquidity_net = tick.liquidity_net.saturating_sub(amount as i128);
            tick.liquidity_gross = tick.liquidity_gross.saturating_add(amount);
        } else {
            self.update_tick(tick_upper, -(amount as i128), amount)?;
        }

        // Update pool liquidity if current tick is in range [tick_lower, tick_upper)
        if self.tick >= tick_lower && self.tick < tick_upper {
            self.liquidity = self.liquidity.saturating_add(amount);
        }

        Ok(())
    }

    /// Helper for applying burn events (R6 refactoring: dedup V3 burn handling)
    fn apply_burn_event(&mut self, tick_lower: i32, tick_upper: i32, amount: u128) -> Result<()> {
        if tick_lower >= tick_upper {
            return Err(anyhow!(
                "Invalid tick range: tick_lower {} >= tick_upper {}",
                tick_lower,
                tick_upper
            ));
        }

        // Check both ends before touching either: failing on the upper tick
        // after the lower one was already debited left the map half-burned.
        if !self.ticks.contains_key(&tick_lower) {
            return Err(anyhow!(
                "Burn attempted on uninitialized tick_lower: {}",
                tick_lower
            ));
        }
        if !self.ticks.contains_key(&tick_upper) {
            return Err(anyhow!(
                "Burn attempted on uninitialized tick_upper: {}",
                tick_upper
            ));
        }

        // Update tick_lower
        if let Some(tick) = self.ticks.get_mut(&tick_lower) {
            let liquidity_net = tick.liquidity_net;
            tick.liquidity_net = tick.liquidity_net.saturating_sub(amount as i128);
            tick.liquidity_gross = tick.liquidity_gross.saturating_sub(amount);
            if tick.liquidity_gross == 0 {
                self.update_tick(tick_lower, liquidity_net, 0)?;
            }
        } else {
            return Err(anyhow!(
                "Burn attempted on uninitialized tick_lower: {}",
                tick_lower
            ));
        }

        // Update tick_upper
        if let Some(tick) = self.ticks.get_mut(&tick_upper) {
            let liquidity_net = tick.liquidity_net;
            tick.liquidity_net = tick.liquidity_net.saturating_add(amount as i128);
            tick.liquidity_gross = tick.liquidity_gross.saturating_sub(amount);
            if tick.liquidity_gross == 0 {
                self.update_tick(tick_upper, liquidity_net, 0)?;
            }
        } else {
            return Err(anyhow!(
                "Burn attempted on uninitialized tick_upper: {}",
                tick_upper
            ));
        }

        // Update pool liquidity if current tick is in range [tick_lower, tick_upper)
        if self.tick >= tick_lower && self.tick < tick_upper {
            self.liquidity = self.liquidity.saturating_sub(amount);
        }

        Ok(())
    }
}

impl PoolInterface for UniswapV3Pool {
    fn calculate_output(&self, token_in: &Address, amount_in: U256) -> Result<U256> {
        self.calculate_exact_input(token_in, amount_in)
    }

    fn calculate_input(&self, token_out: &Address, amount_out: U256) -> Result<U256> {
        self.calculate_exact_output(token_out, amount_out)
    }

    fn apply_swap(&mut self, token_in: &Address, amount_in: U256, amount_out: U256) -> Result<()> {
        self.apply_swap_internal(token_in, amount_in, amount_out)
    }

    fn address(&self) -> Address {
        self.address
    }

    fn tokens(&self) -> (Address, Address) {
        (self.token0, self.token1)
    }

    fn fee(&self) -> f64 {
        self.fee.to::<u128>() as f64 / FEE_DENOMINATOR as f64
    }

    fn fee_raw(&self) -> u64 {
        self.fee.to::<u128>() as u64
    }

    fn id(&self) -> String {
        format!(
            "v3-{}-{}-{}-{}",
            self.address,
            self.token0,
            self.token1,
            self.fee.to::<u128>()
        )
    }

    fn log_summary(&self) -> String {
        format!(
            "V3 Pool {} - {} <> {} (fee: {:.2}%, tick: {}, liquidity: {}, sqrt_price_x96: {}, ticks: {})",
            self.address, self.token0, self.token1, self.fee, self.tick, self.liquidity, self.sqrt_price_x96, self.ticks.len()
        )
    }

    fn contains_token(&self, token: &Address) -> bool {
        *token == self.token0 || *token == self.token1
    }

    fn clone_box(&self) -> Box<dyn PoolInterface + Send + Sync> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl EventApplicable for UniswapV3Pool {
    fn apply_log(&mut self, log: &Log) -> Result<()> {
        match log.topic0() {
            Some(&IUniswapV3Pool::Swap::SIGNATURE_HASH) => {
                let swap_data: IUniswapV3Pool::Swap = log.log_decode()?.inner.data;
                debug!(
                    "Applying V3Swap event to pool {}: sqrt_price_x96={}, tick={}, liquidity={}",
                    self.address, swap_data.sqrtPriceX96, swap_data.tick, swap_data.liquidity
                );
                self.update_state(
                    swap_data.sqrtPriceX96,
                    swap_data.tick.as_i32(),
                    swap_data.liquidity,
                )
            }
            Some(&IPancakeV3Pool::Swap::SIGNATURE_HASH) => {
                let swap_data: IPancakeV3Pool::Swap = log.log_decode()?.inner.data;
                debug!(
                    "Applying V3Swap event to pool {}: sqrt_price_x96={}, tick={}, liquidity={}",
                    self.address, swap_data.sqrtPriceX96, swap_data.tick, swap_data.liquidity
                );
                self.update_state(
                    swap_data.sqrtPriceX96,
                    swap_data.tick.as_i32(),
                    swap_data.liquidity,
                )
            }
            Some(&IAlgebraPoolSei::Swap::SIGNATURE_HASH) => {
                let swap_data: IAlgebraPoolSei::Swap = log.log_decode()?.inner.data;
                debug!(
                    "Applying AlgebraSwap event to pool {}: sqrt_price_x96={}, tick={}, liquidity={}",
                    self.address, swap_data.price, swap_data.tick, swap_data.liquidity
                );
                self.update_state(
                    swap_data.price,
                    swap_data.tick.as_i32(),
                    swap_data.liquidity,
                )
            }
            // A Ramses-family pool changed its fee. Applied here, in log order,
            // rather than read back by a poll after the block: any swap later in
            // the same block executed at the new fee, and a post-block poll
            // prices those swaps on the old one. That ordering gap is exactly
            // what run 6's fork replay caught (5 stale-fee failures in 299).
            Some(&IRamsesCLPool::FeeAdjustment::SIGNATURE_HASH) => {
                let ev: IRamsesCLPool::FeeAdjustment = log.log_decode()?.inner.data;
                // A pool answering `currentFee()` prices swaps at that, not
                // `fee()` (Avalanche 0x0021368B: currentFee() = 75, fee() =
                // 50, both constant over run 8's 12 hours) -- but this event
                // still fires alongside a real change, and applying `newFee`
                // here would overwrite the value that actually matters until
                // the next read. Leave it alone; the pool stays tracked, so
                // the fee reader re-reads `currentFee()` once this batch's
                // touch reaches it (this event is itself a touch).
                if self.fee_source == FeeSource::ReadCurrentFee {
                    debug!(
                        "FeeAdjustment on pool {}: swaps pay currentFee(), not fee() -- left alone",
                        self.address
                    );
                    return Ok(());
                }
                // `oldFee` should equal what we hold. If it does not, an earlier
                // adjustment was missed and the pool was mispriced until now --
                // worth surfacing, since nothing else would.
                if ev.oldFee != self.fee {
                    warn!(
                        "FeeAdjustment on pool {} expected old fee {} but held {}; \
                         an earlier fee change was missed",
                        self.address, ev.oldFee, self.fee
                    );
                }
                // Info, not debug: fee changes are rare and each one reprices every
                // cycle through the pool. The per-block refetch this replaces logged
                // each update at info too, so this keeps the same visibility.
                info!(
                    "Applying FeeAdjustment to pool {}: {} -> {}",
                    self.address, ev.oldFee, ev.newFee
                );
                self.set_fee(ev.newFee);
                Ok(())
            }
            Some(&IUniswapV3Pool::Mint::SIGNATURE_HASH) => {
                let mint_data: IUniswapV3Pool::Mint = log.log_decode()?.inner.data;
                debug!(
                    "Applying V3Mint event to pool {}: tick_lower={}, tick_upper={}, amount={}",
                    self.address, mint_data.tickLower, mint_data.tickUpper, mint_data.amount
                );
                self.apply_mint_event(
                    mint_data.tickLower.as_i32(),
                    mint_data.tickUpper.as_i32(),
                    mint_data.amount,
                )
            }
            Some(&IRamsesCLPool::Mint::SIGNATURE_HASH) => {
                let mint_data: IRamsesCLPool::Mint = log.log_decode()?.inner.data;
                debug!(
                    "Applying RamsesMint event to pool {}: tick_lower={}, tick_upper={}, amount={}",
                    self.address, mint_data.tickLower, mint_data.tickUpper, mint_data.amount
                );
                self.apply_mint_event(
                    mint_data.tickLower.as_i32(),
                    mint_data.tickUpper.as_i32(),
                    mint_data.amount,
                )
            }
            // R6: Deduplicated burn event handling
            Some(&IUniswapV3Pool::Burn::SIGNATURE_HASH) => {
                let burn_data: IUniswapV3Pool::Burn = log.log_decode()?.inner.data;
                debug!(
                    "Applying V3Burn event to pool {}: tick_lower={}, tick_upper={}, amount={}",
                    self.address, burn_data.tickLower, burn_data.tickUpper, burn_data.amount
                );
                self.apply_burn_event(
                    burn_data.tickLower.as_i32(),
                    burn_data.tickUpper.as_i32(),
                    burn_data.amount,
                )
            }
            Some(&IAlgebraPoolSei::Burn::SIGNATURE_HASH) => {
                let burn_data: IAlgebraPoolSei::Burn = log.log_decode()?.inner.data;
                debug!(
                    "Applying AlgebraBurn event to pool {}: tick_lower={}, tick_upper={}, amount={}",
                    self.address,
                    burn_data.bottomTick,
                    burn_data.topTick,
                    burn_data.liquidityAmount
                );
                self.apply_burn_event(
                    burn_data.bottomTick.as_i32(),
                    burn_data.topTick.as_i32(),
                    burn_data.liquidityAmount,
                )
            }
            // The stored fee changed through `setFee`. It is what swaps pay
            // only when the plugin does not compute the fee.
            Some(&IAlgebraIntegralPool::Fee::SIGNATURE_HASH) => {
                let ev: IAlgebraIntegralPool::Fee = log.log_decode()?.inner.data;
                if self.fee_source != FeeSource::ReadFee {
                    info!(
                        "Applying Algebra Fee to pool {}: {} -> {}",
                        self.address, self.fee, ev.fee
                    );
                    self.set_fee(U24::from(ev.fee));
                }
                Ok(())
            }
            // A new plugin or plugin config can switch `fee()` between the
            // stored fee and the plugin's. Forget the classification; the
            // collector queues this pool, and the fee reader re-classifies it
            // with one read.
            Some(&IAlgebraIntegralPool::PluginConfig::SIGNATURE_HASH)
            | Some(&IAlgebraIntegralPool::Plugin::SIGNATURE_HASH) => {
                info!(
                    "Algebra pool {} changed its plugin configuration; fee source reset",
                    self.address
                );
                self.fee_source = FeeSource::Unknown;
                Ok(())
            }
            // Algebra Integral reports what each swap paid. Paying MORE than the
            // fee held means this pool's output would be overstated; paying less
            // (a DEX's own backrunner at 1 ppm, a discount) is harmless.
            Some(&IAlgebraIntegralPool::SwapFee::SIGNATURE_HASH) => {
                let ev: IAlgebraIntegralPool::SwapFee = log.log_decode()?.inner.data;
                let held = self.fee.to::<u32>();
                let override_fee = ev.overrideFee.to::<u32>();
                let base = if override_fee != 0 {
                    override_fee
                } else {
                    held
                };
                let paid = base + ev.pluginFee.to::<u32>();
                self.record_swap_fee(paid > held);
                Ok(())
            }
            _ => {
                trace!("Ignoring non-V3 event for V3 pool");
                Ok(())
            }
        }
    }
}

impl TopicList for UniswapV3Pool {
    fn topics() -> Vec<FixedBytes<32>> {
        vec![
            IUniswapV3Pool::Swap::SIGNATURE_HASH,
            IUniswapV3Pool::Mint::SIGNATURE_HASH,
            IUniswapV3Pool::Burn::SIGNATURE_HASH,
            IPancakeV3Pool::Swap::SIGNATURE_HASH,
            IAlgebraPoolSei::Swap::SIGNATURE_HASH,
            IAlgebraPoolSei::Burn::SIGNATURE_HASH,
            // State, not a trigger: a fee change updates the pool but is not
            // itself a reason to search for a cycle, so it is deliberately absent
            // from `profitable_topics`.
            IRamsesCLPool::FeeAdjustment::SIGNATURE_HASH,
            // Ramses V3 Mint (with position index): state, not a trigger.
            IRamsesCLPool::Mint::SIGNATURE_HASH,
            // Algebra Integral fee state; none is a trigger.
            IAlgebraIntegralPool::Fee::SIGNATURE_HASH,
            IAlgebraIntegralPool::PluginConfig::SIGNATURE_HASH,
            IAlgebraIntegralPool::Plugin::SIGNATURE_HASH,
            IAlgebraIntegralPool::SwapFee::SIGNATURE_HASH,
        ]
    }

    fn profitable_topics() -> Vec<FixedBytes<32>> {
        vec![
            IUniswapV3Pool::Swap::SIGNATURE_HASH,
            IPancakeV3Pool::Swap::SIGNATURE_HASH,
            IAlgebraPoolSei::Swap::SIGNATURE_HASH,
        ]
    }
}

impl fmt::Display for UniswapV3Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "V3 Pool {} - {} <> {} (fee: {:.2}%, tick: {}, liquidity: {})",
            self.address,
            self.token0,
            self.token1,
            (self.fee.to::<u128>() as f64 / FEE_DENOMINATOR as f64) * 100.0,
            self.tick,
            self.liquidity
        )
    }
}

impl PoolTypeTrait for UniswapV3Pool {
    fn pool_type(&self) -> PoolType {
        PoolType::UniswapV3
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256, LogData};

    const TOKEN0: Address = address!("0x0000000000000000000000000000000000000002");
    const TOKEN1: Address = address!("0x0000000000000000000000000000000000000003");

    /// A pool sitting at tick 0 with liquidity on either side.
    fn pool_with_type(pool_type: V3PoolType) -> UniswapV3Pool {
        let mut pool = UniswapV3Pool::new(
            address!("0x0000000000000000000000000000000000000001"),
            TOKEN0,
            TOKEN1,
            U24::from(3000u32),
            60,
            U160::from(Q96_U128), // sqrtPriceX96 at tick 0
            0,
            1_000_000_000_000_000_000u128,
            address!("0x0000000000000000000000000000000000000004"),
            pool_type,
        );
        for (index, liquidity_net) in [
            (-60i32, 1_000_000_000_000_000_000i128),
            (60, -1_000_000_000_000_000_000),
        ] {
            pool.ticks.insert(
                index,
                Tick {
                    index,
                    liquidity_net,
                    liquidity_gross: 1_000_000_000_000_000_000u128,
                },
            );
        }
        pool
    }

    /// Every V3 variant now quotes through the same math. The Ramses
    /// `ratio_conversion_factor` -- a constant calibrated once against the
    /// Ramses quoter to paper over a divergence whose cause turned out to be a
    /// mutable fee -- is gone, so no variant post-scales its output. A
    /// reintroduced scaling branch would break this.
    #[test]
    fn all_v3_variants_quote_identically() {
        let amount_in = U256::from(1_000_000_000_000u64);

        let baseline = pool_with_type(V3PoolType::UniswapV3)
            .calculate_exact_input(&TOKEN0, amount_in)
            .unwrap();

        // Guard against a vacuous 0 == 0 comparison.
        assert!(baseline > U256::ZERO, "fixture produced no output");

        for pool_type in [V3PoolType::PancakeV3, V3PoolType::RamsesCL] {
            let quoted = pool_with_type(pool_type)
                .calculate_exact_input(&TOKEN0, amount_in)
                .unwrap();
            assert_eq!(
                quoted, baseline,
                "{pool_type:?} must quote identically to UniswapV3 -- no post-scaling"
            );
        }
    }

    /// Real topic0 of `FeeAdjustment(uint24,uint24)`, as emitted on Avalanche.
    const FEE_ADJUSTMENT_TOPIC: alloy::primitives::B256 =
        b256!("0x0cba87189055d3b5ab05c96fbd641bc766576c9e7cf0d195bdfb58a0c6a6df24");

    /// The real FeeAdjustment log from 0x71bd7525 at block 96081609 -- the
    /// 800 -> 5500 jump that run 6 priced on the stale 800 and reverted on.
    /// Built from raw bytes exactly as eth_getLogs returned them (topic0 plus
    /// two data words), not via `encode_log_data`, so this proves the binding
    /// decodes what the chain actually emits rather than round-tripping itself.
    fn real_fee_adjustment_log() -> Log {
        let mut data = vec![0u8; 64];
        data[30..32].copy_from_slice(&800u16.to_be_bytes()); // oldFee
        data[62..64].copy_from_slice(&5500u16.to_be_bytes()); // newFee
        Log {
            inner: alloy::primitives::Log {
                address: address!("0x71bd752508936dea5a032991f4a2997a506b1cde"),
                data: LogData::new_unchecked(vec![FEE_ADJUSTMENT_TOPIC], data.into()),
            },
            ..Default::default()
        }
    }

    #[test]
    fn fee_adjustment_topic_matches_deployed_contract() {
        assert_eq!(
            IRamsesCLPool::FeeAdjustment::SIGNATURE_HASH,
            FEE_ADJUSTMENT_TOPIC
        );
    }

    #[test]
    fn fee_adjustment_applies_the_new_fee() {
        let mut pool = pool_with_type(V3PoolType::RamsesCL);
        pool.set_fee(U24::from(800u32));
        pool.apply_log(&real_fee_adjustment_log()).unwrap();
        assert_eq!(pool.fee, U24::from(5500u32));
    }

    /// If an earlier adjustment was missed, the pool holds a fee that disagrees
    /// with this event's `oldFee`. The event is still the authority on the new
    /// fee -- the mismatch is warned about, never allowed to strand the pool on
    /// a stale value.
    #[test]
    fn fee_adjustment_recovers_from_a_missed_earlier_change() {
        let mut pool = pool_with_type(V3PoolType::RamsesCL); // holds 3000
        pool.apply_log(&real_fee_adjustment_log()).unwrap(); // event says old was 800
        assert_eq!(pool.fee, U24::from(5500u32));
    }

    /// A pool answering `currentFee()` prices swaps at that, not `fee()` --
    /// applying this event's `newFee` would overwrite the value that
    /// actually matters until the fee reader's next read. It must also not
    /// warn about a mismatched `oldFee`: that field tracks `fee()`, which for
    /// such a pool disagrees with the held `currentFee()` value routinely.
    #[test]
    fn fee_adjustment_leaves_a_current_fee_pool_alone() {
        let mut pool = pool_with_type(V3PoolType::RamsesCL);
        pool.fee_source = FeeSource::ReadCurrentFee;
        pool.set_fee(U24::from(75u32));
        pool.apply_log(&real_fee_adjustment_log()).unwrap();
        assert_eq!(pool.fee, U24::from(75u32));
    }

    /// A fee change is state, not an opportunity: it must be fetched so it is
    /// applied, but must not by itself kick off a cycle search.
    #[test]
    fn fee_adjustment_is_fetched_but_does_not_trigger_a_search() {
        let topic = IRamsesCLPool::FeeAdjustment::SIGNATURE_HASH;
        assert!(UniswapV3Pool::topics().contains(&topic), "must be fetched");
        assert!(
            !UniswapV3Pool::profitable_topics().contains(&topic),
            "must not trigger a search"
        );
    }

    /// topic0 of Ramses V3's
    /// `Mint(address,address,uint256,int24,int24,uint128,uint256,uint256)`:
    /// Uniswap V3's Mint with the NFT position `index` added, so its hash differs.
    const RAMSES_MINT_TOPIC: alloy::primitives::B256 =
        b256!("0xd78218c0d304e8893cb3200abe394bbc8d5b7804d9c51f236df9fdcf481d02d3");

    fn uint_word(v: u128) -> [u8; 32] {
        U256::from(v).to_be_bytes::<32>()
    }

    /// A signed 32-byte word, sign-extended the way the ABI encodes int24.
    fn int_word(v: i64) -> alloy::primitives::B256 {
        let mut word = if v < 0 { [0xffu8; 32] } else { [0u8; 32] };
        word[24..].copy_from_slice(&v.to_be_bytes());
        alloy::primitives::B256::from(word)
    }

    /// The real Ramses V3 Mint on 0xf01449c0 at block 96,213,057 (tx 0xcc5a889f…):
    /// owner and both ticks are indexed; sender, index, amount, amount0 and
    /// amount1 are data. Raw bytes as eth_getLogs returned them, so this proves
    /// the binding decodes what the chain emits.
    fn real_ramses_mint_log() -> Log {
        let owner = address!("0x0b4478e810d48b5882d4019d435a2f864bab4f39");
        let owner_word = alloy::primitives::B256::left_padding_from(owner.as_slice());
        let mut data = Vec::with_capacity(5 * 32);
        data.extend_from_slice(owner_word.as_slice()); // sender
        data.extend_from_slice(&uint_word(605_859)); // index
        data.extend_from_slice(&uint_word(1_166_100_850_557_528_561)); // amount
        data.extend_from_slice(&uint_word(40_491_282_879_675_690_316)); // amount0
        data.extend_from_slice(&uint_word(1_477_611_480)); // amount1
        Log {
            inner: alloy::primitives::Log {
                address: address!("0xf01449c0ba930b6e2caca3def3ccbd7a3e589534"),
                data: LogData::new_unchecked(
                    vec![
                        RAMSES_MINT_TOPIC,
                        owner_word,
                        int_word(-252_550),
                        int_word(-252_540),
                    ],
                    data.into(),
                ),
            },
            ..Default::default()
        }
    }

    #[test]
    fn ramses_mint_topic_matches_deployed_contract() {
        assert_eq!(IRamsesCLPool::Mint::SIGNATURE_HASH, RAMSES_MINT_TOPIC);
    }

    /// Run 8 applied every Ramses Burn but no Ramses Mint -- the topic was not
    /// even fetched -- so positions minted after startup were missing and their
    /// burns ate into live ticks: 588 "Burn attempted on uninitialized tick"
    /// errors in 12 h, and a live tick missing next to the price in three
    /// replayed opportunities (bulk_031#3/#4, bulk_040#55).
    #[test]
    fn ramses_mint_adds_the_position() {
        let mut pool = pool_with_type(V3PoolType::RamsesCL); // at tick 0
        let liquidity_before = pool.liquidity;

        pool.apply_log(&real_ramses_mint_log()).unwrap();

        let amount = 1_166_100_850_557_528_561u128;
        let lower = pool.ticks.get(&-252_550).expect("lower tick added");
        let upper = pool.ticks.get(&-252_540).expect("upper tick added");
        assert_eq!(
            (lower.liquidity_net, lower.liquidity_gross),
            (amount as i128, amount)
        );
        assert_eq!(
            (upper.liquidity_net, upper.liquidity_gross),
            (-(amount as i128), amount)
        );
        assert_eq!(
            pool.liquidity, liquidity_before,
            "the range lies below tick 0"
        );
    }

    #[test]
    fn ramses_mint_is_fetched_but_does_not_trigger_a_search() {
        let topic = IRamsesCLPool::Mint::SIGNATURE_HASH;
        assert!(UniswapV3Pool::topics().contains(&topic), "must be fetched");
        assert!(
            !UniswapV3Pool::profitable_topics().contains(&topic),
            "must not trigger a search"
        );
    }

    /// A burn on a range whose upper tick the pool does not hold used to debit
    /// the lower tick before failing, leaving the map half-burned.
    #[test]
    fn a_burn_on_an_unknown_tick_changes_nothing() {
        let mut pool = pool_with_type(V3PoolType::RamsesCL); // ticks at -60 and 60
        let before = pool.ticks.get(&-60).cloned().unwrap();

        assert!(pool.apply_burn_event(-60, 120, 1_000).is_err());

        let after = pool.ticks.get(&-60).unwrap();
        assert_eq!(
            (after.liquidity_net, after.liquidity_gross),
            (before.liquidity_net, before.liquidity_gross)
        );
    }

    /// Pangolin V3 PNG/USDt 0xd3e0…5c32 (fee 10, spacing 1). No initialized
    /// tick lies within six words either side at these blocks, so the pool's
    /// liquidity is modelled as one full-range position.
    fn pangolin_png_usdt(pool_type: V3PoolType, sqrt_price_x96: u128, tick: i32) -> UniswapV3Pool {
        const LIQUIDITY: u128 = 40_771_086_622_940_920;
        let mut pool = UniswapV3Pool::new(
            address!("0xd3e0B1D5a7f225498f2c1E88e1377c54A7925c32"),
            TOKEN0, // PNG
            TOKEN1, // USDt
            U24::from(10u32),
            1,
            U160::from(sqrt_price_x96),
            tick,
            LIQUIDITY,
            Address::ZERO,
            pool_type,
        );
        for (index, liquidity_net) in [
            (-887_272i32, LIQUIDITY as i128),
            (887_272, -(LIQUIDITY as i128)),
        ] {
            pool.ticks.insert(
                index,
                Tick {
                    index,
                    liquidity_net,
                    liquidity_gross: LIQUIDITY,
                },
            );
        }
        pool
    }

    /// Block 96,207,105 (run 8 fixture bulk_011#147): 8,172,733 USDt in, 19
    /// ticks below the end of its bitmap word. On chain the swap ends a step at
    /// tick -310785 and carries on; the pool paid 256,578,681,201,515,781,113 PNG.
    /// Skipping the edge quoted 256,578,691,550,243,836,051.
    #[test]
    fn uniswap_v3_stops_at_word_edges_one_for_zero() {
        let pool = pangolin_png_usdt(
            V3PoolType::UniswapV3,
            14_132_105_716_635_770_162_996,
            -310_804,
        );
        let out = pool
            .calculate_exact_input(&TOKEN1, U256::from(8_172_733u64))
            .unwrap();
        assert_eq!(out, U256::from(256_578_681_201_515_781_113u128));
    }

    /// Block 96,191,846 (fixture bulk_001#349): 215,837,973,359,182,878,854 PNG
    /// in, 6 ticks above the start of its word (tick -310528). The pool paid
    /// 7,056,618 USDt; skipping the edge quoted 7,056,619.
    #[test]
    fn uniswap_v3_stops_at_word_edges_zero_for_one() {
        let pool = pangolin_png_usdt(
            V3PoolType::UniswapV3,
            14_332_568_801_641_688_341_923,
            -310_522,
        );
        let out = pool
            .calculate_exact_input(&TOKEN0, U256::from(215_837_973_359_182_878_854u128))
            .unwrap();
        assert_eq!(out, U256::from(7_056_618u64));
    }

    /// Algebra Integral walks a linked list of initialized ticks and never
    /// stops at a word edge; its quote must not change.
    #[test]
    fn algebra_steps_straight_to_the_next_initialized_tick() {
        let pool = pangolin_png_usdt(
            V3PoolType::AlgebraV3,
            14_132_105_716_635_770_162_996,
            -310_804,
        );
        let out = pool
            .calculate_exact_input(&TOKEN1, U256::from(8_172_733u64))
            .unwrap();
        assert_eq!(out, U256::from(256_578_691_550_243_836_051u128));
    }

    /// Review regression: an AlgebraV3 pool with liquidity above the current
    /// tick but no initialized tick above it (e.g. a map that lost its upper
    /// tick). A oneForZero quote used to spin forever: `next_initialized_tick`'s
    /// `NextInitialized` arm falls back to the current tick itself when
    /// nothing is initialized above, and an upward crossing does not
    /// decrement it, so the old guard accepted that zero-amount step as
    /// "reached its target" and looped on the same tick forever. It must
    /// fail promptly instead.
    #[test]
    fn one_for_zero_errors_instead_of_spinning_with_nothing_initialized_above() {
        const LIQUIDITY: u128 = 1_000_000_000_000_000_000;
        let mut pool = UniswapV3Pool::new(
            address!("0x00000000000000000000000000000000000000aa"),
            TOKEN0,
            TOKEN1,
            U24::from(500u32),
            60,
            U160::from(Q96_U128),
            0,
            LIQUIDITY,
            Address::ZERO,
            V3PoolType::AlgebraV3,
        );
        pool.ticks.insert(
            -600,
            Tick {
                index: -600,
                liquidity_net: LIQUIDITY as i128,
                liquidity_gross: LIQUIDITY,
            },
        );

        assert!(pool
            .calculate_exact_input(&TOKEN1, U256::from(1_000_000u64))
            .is_err());
    }

    /// Every `V3PoolType` maps to exactly the search its on-chain swap loop
    /// uses; a new variant added without updating this match, and this test,
    /// would silently misprice it.
    #[test]
    fn tick_search_covers_every_pool_type() {
        for pool_type in [
            V3PoolType::UniswapV3,
            V3PoolType::PancakeV3,
            V3PoolType::RamsesCL,
            V3PoolType::AlgebraTwoSideFee,
            V3PoolType::AlgebraPoolFeeInState,
        ] {
            assert_eq!(
                pool_with_type(pool_type).tick_search(),
                TickSearch::WordBounded { tick_spacing: 60 },
                "{pool_type:?} must be word-bounded"
            );
        }
        assert_eq!(
            pool_with_type(V3PoolType::AlgebraV3).tick_search(),
            TickSearch::NextInitialized
        );
    }

    #[test]
    fn fee_source_follows_what_the_pool_reports() {
        use V3PoolType::*;
        // pluginConfig 215 (Flare SparkDEX) has DYNAMIC_FEE; 2 and 87 (Avalanche) do not.
        assert_eq!(
            classify_fee_source(AlgebraV3, Some(215), false),
            FeeSource::ReadFee
        );
        assert_eq!(
            classify_fee_source(AlgebraV3, Some(2), false),
            FeeSource::Events
        );
        assert_eq!(
            classify_fee_source(AlgebraV3, Some(87), false),
            FeeSource::Events
        );
        assert_eq!(
            classify_fee_source(AlgebraV3, None, false),
            FeeSource::Unknown
        );
        assert_eq!(
            classify_fee_source(RamsesCL, None, true),
            FeeSource::ReadCurrentFee
        );
        assert_eq!(
            classify_fee_source(RamsesCL, None, false),
            FeeSource::Events
        );
        assert_eq!(
            classify_fee_source(UniswapV3, None, false),
            FeeSource::Events
        );
    }

    /// `fee_source` is runtime state: it must not change how a pool persists,
    /// and a restored pool must come back unclassified.
    #[test]
    fn fee_source_is_not_persisted() {
        let mut pool = pool_with_type(V3PoolType::AlgebraV3);
        pool.fee_source = FeeSource::ReadFee;
        let json = serde_json::to_value(&pool).unwrap();
        assert!(json.get("fee_source").is_none());
        assert!(json.get("swap_fee_misses").is_none());
        let restored: UniswapV3Pool = serde_json::from_value(json).unwrap();
        assert_eq!(restored.fee_source, FeeSource::Unknown);
    }

    const ALGEBRA_FEE_TOPIC: alloy::primitives::B256 =
        b256!("0x598b9f043c813aa6be3426ca60d1c65d17256312890be5118dab55b0775ebe2a");
    const ALGEBRA_PLUGIN_CONFIG_TOPIC: alloy::primitives::B256 =
        b256!("0x3a6271b36c1b44bd6a0a0d56230602dc6919b7c17af57254306fadf5fee69dc3");
    const ALGEBRA_PLUGIN_TOPIC: alloy::primitives::B256 =
        b256!("0x27a3944eff2135a57675f17e72501038982b73620d01f794c72e93d61a3932a2");

    fn one_word_log(topic: alloy::primitives::B256, word: [u8; 32]) -> Log {
        Log {
            inner: alloy::primitives::Log {
                address: address!("0x0000000000000000000000000000000000000001"),
                data: LogData::new_unchecked(vec![topic], word.to_vec().into()),
            },
            ..Default::default()
        }
    }

    const SWAP_FEE_TOPIC: alloy::primitives::B256 =
        b256!("0x9443903d84c9719611bd4bba871daaf18a3950d00d5d78b1a2fa701f76df54ff");

    fn swap_fee_log(override_fee: u32, plugin_fee: u32) -> Log {
        let sender = address!("0x0000000000000000000000000000000000000009");
        let mut data = uint_word(override_fee as u128).to_vec();
        data.extend_from_slice(&uint_word(plugin_fee as u128));
        Log {
            inner: alloy::primitives::Log {
                address: address!("0x0000000000000000000000000000000000000001"),
                data: LogData::new_unchecked(
                    vec![
                        SWAP_FEE_TOPIC,
                        alloy::primitives::B256::left_padding_from(sender.as_slice()),
                    ],
                    data.into(),
                ),
            },
            ..Default::default()
        }
    }

    #[test]
    fn algebra_event_topics_match_deployed_contracts() {
        assert_eq!(IAlgebraIntegralPool::Fee::SIGNATURE_HASH, ALGEBRA_FEE_TOPIC);
        assert_eq!(
            IAlgebraIntegralPool::PluginConfig::SIGNATURE_HASH,
            ALGEBRA_PLUGIN_CONFIG_TOPIC
        );
        assert_eq!(
            IAlgebraIntegralPool::Plugin::SIGNATURE_HASH,
            ALGEBRA_PLUGIN_TOPIC
        );
    }

    /// Avalanche 0x259d… at block 96,050,122: Fee(500), down from 5000. With
    /// DYNAMIC_FEE off this is the only way the fee moves.
    #[test]
    fn an_algebra_fee_event_sets_a_stored_fee() {
        let mut pool = pool_with_type(V3PoolType::AlgebraV3);
        pool.fee_source = FeeSource::Events;
        pool.set_fee(U24::from(5000u32));
        pool.apply_log(&one_word_log(ALGEBRA_FEE_TOPIC, uint_word(500)))
            .unwrap();
        assert_eq!(pool.fee, U24::from(500u32));
    }

    /// With DYNAMIC_FEE on, `fee()` is the plugin's; the stored fee this event
    /// reports is not what swaps pay and must not overwrite the read value.
    #[test]
    fn an_algebra_fee_event_is_ignored_when_the_plugin_sets_the_fee() {
        let mut pool = pool_with_type(V3PoolType::AlgebraV3);
        pool.fee_source = FeeSource::ReadFee;
        pool.set_fee(U24::from(222u32));
        pool.apply_log(&one_word_log(ALGEBRA_FEE_TOPIC, uint_word(500)))
            .unwrap();
        assert_eq!(pool.fee, U24::from(222u32));
    }

    /// Flare 0x1922… at block 70,513,172: Plugin(new), PluginConfig(0),
    /// PluginConfig(215). Either event can switch `fee()` between the stored
    /// fee and the plugin's, so the pool forgets its classification.
    #[test]
    fn a_plugin_change_resets_the_fee_source() {
        for topic in [ALGEBRA_PLUGIN_TOPIC, ALGEBRA_PLUGIN_CONFIG_TOPIC] {
            let mut pool = pool_with_type(V3PoolType::AlgebraV3);
            pool.fee_source = FeeSource::Events;
            pool.apply_log(&one_word_log(topic, uint_word(215)))
                .unwrap();
            assert_eq!(pool.fee_source, FeeSource::Unknown);
        }
    }

    #[test]
    fn swap_fee_topic_matches_deployed_contract() {
        assert_eq!(
            IAlgebraIntegralPool::SwapFee::SIGNATURE_HASH,
            SWAP_FEE_TOPIC
        );
    }

    /// Flare 0x1922… (SparkDEX FTSO-PMM): `fee()` reads the 1000 floor while
    /// swaps in the arbitrage direction paid up to 7400.
    #[test]
    fn a_pool_whose_swaps_keep_paying_more_stops_quoting() {
        let mut pool = pool_with_type(V3PoolType::AlgebraV3);
        pool.set_fee(U24::from(1000u32));
        for paid in [2600u32, 1, 4600, 2400] {
            pool.apply_log(&swap_fee_log(paid, 0)).unwrap();
        }
        assert!(pool.fee_unpredictable());
        assert!(pool
            .calculate_exact_input(&TOKEN0, U256::from(1_000_000u64))
            .is_err());
    }

    /// A DEX's own backrunner at 1 ppm, discounts, and swaps at the pool fee
    /// paid no more than the fee held: never a miss.
    #[test]
    fn swaps_paying_the_fee_or_less_do_not_count() {
        let mut pool = pool_with_type(V3PoolType::AlgebraV3);
        pool.set_fee(U24::from(100u32));
        for (override_fee, plugin_fee) in [(1u32, 0u32), (100, 0), (0, 0), (60, 0), (1, 0)] {
            pool.apply_log(&swap_fee_log(override_fee, plugin_fee))
                .unwrap();
        }
        assert!(!pool.fee_unpredictable());
        assert!(pool
            .calculate_exact_input(&TOKEN0, U256::from(1_000_000u64))
            .is_ok());
    }

    /// A plugin fee is charged on top of the pool fee.
    #[test]
    fn a_plugin_fee_on_top_counts_as_paying_more() {
        let mut pool = pool_with_type(V3PoolType::AlgebraV3);
        pool.set_fee(U24::from(100u32));
        for _ in 0..3 {
            pool.apply_log(&swap_fee_log(0, 50)).unwrap();
        }
        assert!(pool.fee_unpredictable());
    }

    /// Quoting resumes once the misses age out of the window.
    #[test]
    fn quoting_resumes_when_misses_age_out() {
        let mut pool = pool_with_type(V3PoolType::AlgebraV3);
        pool.set_fee(U24::from(1000u32));
        for _ in 0..3 {
            pool.apply_log(&swap_fee_log(2000, 0)).unwrap();
        }
        assert!(pool.fee_unpredictable());
        for _ in 0..8 {
            pool.apply_log(&swap_fee_log(1000, 0)).unwrap();
        }
        assert!(
            !pool.fee_unpredictable(),
            "2 misses left in the last 10 swaps"
        );
    }

    #[test]
    fn algebra_fee_events_are_fetched_but_do_not_trigger_a_search() {
        for topic in [
            IAlgebraIntegralPool::Fee::SIGNATURE_HASH,
            IAlgebraIntegralPool::PluginConfig::SIGNATURE_HASH,
            IAlgebraIntegralPool::Plugin::SIGNATURE_HASH,
            IAlgebraIntegralPool::SwapFee::SIGNATURE_HASH,
        ] {
            assert!(UniswapV3Pool::topics().contains(&topic), "must be fetched");
            assert!(!UniswapV3Pool::profitable_topics().contains(&topic));
        }
    }
}
