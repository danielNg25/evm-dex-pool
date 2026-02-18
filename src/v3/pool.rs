use crate::contracts::{IAlgebraPoolSei, IPancakeV3Pool, IUniswapV3Pool};
use crate::pool::base::{EventApplicable, PoolInterface, PoolType, PoolTypeTrait, TopicList};
use alloy::primitives::{aliases::U24, Address, Signed, U160, U256};
use alloy::primitives::FixedBytes;
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;
use anyhow::{anyhow, Result};
use log::{debug, trace};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::{collections::BTreeMap, fmt};

use super::{v3_swap, Tick, TickMap};

/// The Q64.96 precision used by Uniswap V3
pub const Q96_U128: u128 = 1 << 96;
pub const FEE_DENOMINATOR: u32 = 1000000;
pub const RAMSES_FACTOR: u128 = 10000000000;

/// Enum representing the type of V3 pool
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum V3PoolType {
    UniswapV3,
    PancakeV3,
    AlgebraV3,
    RamsesV2,
    AlgebraTwoSideFee,
    AlgebraPoolFeeInState,
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
    /// Ratio conversion factor
    pub ratio_conversion_factor: U256,
    /// Factory address
    pub factory: Address,
    /// Last update timestamp
    pub last_updated: u64,
    /// Creation timestamp or block
    pub created_at: u64,
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
            ratio_conversion_factor: U256::from(RAMSES_FACTOR),
            factory,
        }
    }

    pub fn update_ratio_conversion_factor(&mut self, factor: U256) {
        self.ratio_conversion_factor = factor;
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
        let result;
        if token_in == &self.token0 {
            result = self.calculate_zero_for_one(amount_in, true)?;
        } else if token_in == &self.token1 {
            result = self.calculate_one_for_zero(amount_in, true)?;
        } else {
            return Err(anyhow!("Token not in pool"));
        }
        if self.pool_type == V3PoolType::RamsesV2 {
            Ok(result * self.ratio_conversion_factor / U256::from(RAMSES_FACTOR))
        } else {
            Ok(result)
        }
    }

    /// Calculate the amount out for a swap with the exact formula
    pub fn calculate_exact_output(&self, token_out: &Address, amount_in: U256) -> Result<U256> {
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

    /// Helper for applying burn events (R6 refactoring: dedup V3 burn handling)
    fn apply_burn_event(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        amount: u128,
    ) -> Result<()> {
        if tick_lower >= tick_upper {
            return Err(anyhow!(
                "Invalid tick range: tick_lower {} >= tick_upper {}",
                tick_lower,
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
            Some(&IUniswapV3Pool::Mint::SIGNATURE_HASH) => {
                let mint_data: IUniswapV3Pool::Mint = log.log_decode()?.inner.data;
                debug!(
                    "Applying V3Mint event to pool {}: tick_lower={}, tick_upper={}, amount={}",
                    self.address, mint_data.tickLower, mint_data.tickUpper, mint_data.amount
                );

                let amount_u128 = mint_data.amount;
                let tick_lower_i32 = mint_data.tickLower.as_i32();
                let tick_upper_i32 = mint_data.tickUpper.as_i32();

                if tick_lower_i32 >= tick_upper_i32 {
                    return Err(anyhow!(
                        "Invalid tick range: tick_lower {} >= tick_upper {}",
                        tick_lower_i32,
                        tick_upper_i32
                    ));
                }

                // Update tick_lower
                if let Some(tick) = self.ticks.get_mut(&tick_lower_i32) {
                    tick.liquidity_net = tick.liquidity_net.saturating_add(amount_u128 as i128);
                    tick.liquidity_gross = tick.liquidity_gross.saturating_add(amount_u128);
                } else {
                    self.update_tick(tick_lower_i32, amount_u128 as i128, amount_u128)?;
                }

                // Update tick_upper
                if let Some(tick) = self.ticks.get_mut(&tick_upper_i32) {
                    tick.liquidity_net = tick.liquidity_net.saturating_sub(amount_u128 as i128);
                    tick.liquidity_gross = tick.liquidity_gross.saturating_add(amount_u128);
                } else {
                    self.update_tick(tick_upper_i32, -(amount_u128 as i128), amount_u128)?;
                }

                // Update pool liquidity if current tick is in range [tick_lower, tick_upper)
                if self.tick >= tick_lower_i32 && self.tick < tick_upper_i32 {
                    self.liquidity = self.liquidity.saturating_add(amount_u128);
                }

                Ok(())
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
