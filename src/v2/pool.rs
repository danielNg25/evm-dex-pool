use crate::contracts::{IUniswapV2Pair, IV2PairUint256};
use crate::pool::base::{EventApplicable, PoolInterface, PoolTypeTrait, TopicList};
use crate::pool::PoolType;
use alloy::sol_types::SolEvent;
use alloy::{
    primitives::{Address, FixedBytes, U256},
    rpc::types::Log,
};
use anyhow::{anyhow, Result};
use log::{debug, trace};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::fmt;

const FEE_DENOMINATOR: u128 = 1000000;
const EXP18: u128 = 1_000_000_000_000_000_000;

/// Enum representing the type of V2 pool
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum V2PoolType {
    UniswapV2,
    Stable,
}

/// UniswapV2 Pool implementation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniswapV2Pool {
    /// Pool type
    pub pool_type: V2PoolType,
    /// Pool address
    pub address: Address,
    /// First token address in the pool
    pub token0: Address,
    /// Second token address in the pool
    pub token1: Address,
    /// Decimals of token0
    pub decimals0: u128,
    /// Decimals of token1
    pub decimals1: u128,
    /// Reserve of token0
    pub reserve0: U256,
    /// Reserve of token1
    pub reserve1: U256,
    /// Pool fee (e.g., 0.003 for 0.3%)
    pub fee: U256,
    /// Last update timestamp
    pub last_updated: u64,
    /// Creation timestamp or block
    pub created_at: u64,
}

impl UniswapV2Pool {
    /// Create a new V2 pool
    pub fn new(
        pool_type: V2PoolType,
        address: Address,
        token0: Address,
        token1: Address,
        decimals_0: u8,
        decimals_1: u8,
        reserve0: U256,
        reserve1: U256,
        fee: U256,
    ) -> Self {
        let current_time = chrono::Utc::now().timestamp() as u64;
        Self {
            pool_type,
            address,
            token0,
            token1,
            decimals0: 10_u128.pow(decimals_0 as u32),
            decimals1: 10_u128.pow(decimals_1 as u32),
            reserve0,
            reserve1,
            fee,
            last_updated: current_time,
            created_at: current_time,
        }
    }

    /// Update pool reserves
    pub fn update_reserves(&mut self, reserve0: U256, reserve1: U256) -> Result<()> {
        self.reserve0 = reserve0;
        self.reserve1 = reserve1;
        self.last_updated = chrono::Utc::now().timestamp() as u64;
        Ok(())
    }

    /// Calculate the constant product k = x * y
    pub fn constant_product(&self) -> U256 {
        self.reserve0 * self.reserve1
    }

    /// Get the exp18 value
    pub fn exp18() -> U256 {
        U256::from(EXP18)
    }

    /// Check if the pool is valid (has non-zero reserves)
    pub fn is_valid(&self) -> bool {
        !self.reserve0.is_zero() && !self.reserve1.is_zero()
    }

    /// Calculate the output amount for a swap (token0 -> token1)
    fn calculate_output_0_to_1(&self, amount_in: U256) -> Result<U256> {
        if amount_in.is_zero() {
            return Err(anyhow!("Input amount cannot be zero"));
        }

        if !self.is_valid() {
            return Err(anyhow!("Pool reserves are invalid"));
        }
        match self.pool_type {
            V2PoolType::UniswapV2 => {
                let amount_in_with_fee: alloy::primitives::Uint<256, 4> =
                    amount_in.saturating_mul(U256::from(U256::from(FEE_DENOMINATOR) - (self.fee)));
                let numerator = amount_in_with_fee * self.reserve1;
                let denominator = self.reserve0 * U256::from(FEE_DENOMINATOR) + amount_in_with_fee;
                let output = numerator / denominator;
                if output >= self.reserve1 {
                    return Err(anyhow!("Insufficient liquidity for swap"));
                }
                Ok(output)
            }
            V2PoolType::Stable => {
                self.calculate_stable_output(amount_in, true)
            }
        }
    }

    /// Calculate the output amount for a swap (token1 -> token0)
    fn calculate_output_1_to_0(&self, amount_in: U256) -> Result<U256> {
        if amount_in.is_zero() {
            return Err(anyhow!("Input amount cannot be zero"));
        }

        if !self.is_valid() {
            return Err(anyhow!("Pool reserves are invalid"));
        }

        match self.pool_type {
            V2PoolType::UniswapV2 => {
                let amount_in_with_fee =
                    amount_in.saturating_mul(U256::from(U256::from(FEE_DENOMINATOR) - (self.fee)));
                let numerator = amount_in_with_fee * self.reserve0;
                let denominator = self.reserve1 * U256::from(FEE_DENOMINATOR) + amount_in_with_fee;
                let output = numerator / denominator;
                if output >= self.reserve0 {
                    return Err(anyhow!("Insufficient liquidity for swap"));
                }
                Ok(output)
            }
            V2PoolType::Stable => {
                self.calculate_stable_output(amount_in, false)
            }
        }
    }

    /// Unified stable swap output calculation (R7 refactoring)
    fn calculate_stable_output(&self, amount_in: U256, zero_for_one: bool) -> Result<U256> {
        let amount_in_with_fee: alloy::primitives::Uint<256, 4> = amount_in
            .saturating_mul(U256::from(U256::from(FEE_DENOMINATOR) - (self.fee)))
            .checked_div(U256::from(FEE_DENOMINATOR))
            .unwrap();
        let exp18 = Self::exp18();
        let xy = self.k(self.reserve0, self.reserve1);
        let reserve0 = (self.reserve0 * exp18) / U256::from(self.decimals0);
        let reserve1 = (self.reserve1 * exp18) / U256::from(self.decimals1);

        let (reserve_a, reserve_b, decimals_in, decimals_out) = if zero_for_one {
            (reserve0, reserve1, self.decimals0, self.decimals1)
        } else {
            (reserve1, reserve0, self.decimals1, self.decimals0)
        };

        let amount_in_parsed = (amount_in_with_fee * exp18) / U256::from(decimals_in);
        let y = reserve_b - self.get_y(amount_in_parsed + reserve_a, xy, reserve_b)?;
        let output = (y * U256::from(decimals_out)) / exp18;

        Ok(output)
    }

    fn k(&self, x: U256, y: U256) -> U256 {
        if self.pool_type == V2PoolType::Stable {
            let exp18 = Self::exp18();
            let _x = (x * exp18) / U256::from(self.decimals0);
            let _y = (y * exp18) / U256::from(self.decimals1);
            let _a = (_x * _y) / exp18;
            let _b = (_x * _x) / exp18 + (_y * _y) / exp18;
            return (_a * _b) / exp18; // x3y+y3x >= k
        } else {
            return x * y;
        }
    }

    pub fn f(x0: U256, y: U256) -> U256 {
        let exp18 = Self::exp18();
        let _a = (x0 * y) / exp18;
        let _b = (x0 * x0) / exp18 + (y * y) / exp18;
        return (_a * _b) / exp18;
    }

    fn d(x0: U256, y: U256) -> U256 {
        let exp18 = Self::exp18();
        return (U256::from(3) * x0 * ((y * y) / exp18)) / exp18
            + ((((x0 * x0) / exp18) * x0) / exp18);
    }

    fn get_y(&self, x0: U256, xy: U256, mut y: U256) -> Result<U256> {
        let exp18 = Self::exp18();
        for _ in 0..255 {
            let k = Self::f(x0, y);
            if k < xy {
                let mut dy = ((xy - k) * exp18) / Self::d(x0, y);
                if dy.is_zero() {
                    if k == xy {
                        return Ok(y);
                    }
                    if self.k(x0, y + U256::ONE) > xy {
                        return Ok(y + U256::ONE);
                    }
                    dy = U256::ONE;
                }
                y = y + dy;
            } else {
                let mut dy = ((k - xy) * exp18) / Self::d(x0, y);
                if dy.is_zero() {
                    if k == xy || Self::f(x0, y - U256::ONE) < xy {
                        return Ok(y);
                    }
                    dy = U256::ONE;
                }
                y = y - dy;
            }
        }
        return Err(anyhow!("!y"));
    }

    fn calculate_input_0_to_1(&self, amount_out: U256) -> Result<U256> {
        if amount_out.is_zero() {
            return Err(anyhow!("Output amount cannot be zero"));
        }
        if !self.is_valid() {
            return Err(anyhow!("Pool reserves are invalid"));
        }
        if amount_out >= self.reserve1 {
            return Err(anyhow!("Insufficient liquidity for swap"));
        }
        let numerator = self.reserve0 * amount_out * U256::from(FEE_DENOMINATOR);
        let denominator = (self.reserve1 - amount_out) * (U256::from(FEE_DENOMINATOR) - self.fee);
        let input = (numerator / denominator) + U256::from(1);
        Ok(input)
    }

    fn calculate_input_1_to_0(&self, amount_out: U256) -> Result<U256> {
        if amount_out.is_zero() {
            return Err(anyhow!("Output amount cannot be zero"));
        }
        if !self.is_valid() {
            return Err(anyhow!("Pool reserves are invalid"));
        }
        if amount_out >= self.reserve0 {
            return Err(anyhow!("Insufficient liquidity for swap"));
        }
        let numerator = self.reserve1 * amount_out * U256::from(FEE_DENOMINATOR);
        let denominator = (self.reserve0 - amount_out) * (U256::from(FEE_DENOMINATOR) - self.fee);
        let input = (numerator / denominator) + U256::from(1);
        Ok(input)
    }
}

impl PoolInterface for UniswapV2Pool {
    fn calculate_output(&self, token_in: &Address, amount_in: U256) -> Result<U256> {
        if token_in == &self.token0 {
            self.calculate_output_0_to_1(amount_in)
        } else if token_in == &self.token1 {
            self.calculate_output_1_to_0(amount_in)
        } else {
            Err(anyhow!("Token not in pool"))
        }
    }

    fn calculate_input(&self, token_out: &Address, amount_out: U256) -> Result<U256> {
        if token_out == &self.token0 {
            self.calculate_input_1_to_0(amount_out)
        } else if token_out == &self.token1 {
            self.calculate_input_0_to_1(amount_out)
        } else {
            Err(anyhow!("Token not in pool"))
        }
    }

    fn apply_swap(&mut self, token_in: &Address, amount_in: U256, amount_out: U256) -> Result<()> {
        if token_in == &self.token0 {
            if amount_out >= self.reserve1 {
                return Err(anyhow!("Insufficient liquidity for swap"));
            }
            self.reserve0 += amount_in;
            self.reserve1 -= amount_out;
        } else if token_in == &self.token1 {
            if amount_out >= self.reserve0 {
                return Err(anyhow!("Insufficient liquidity for swap"));
            }
            self.reserve1 += amount_in;
            self.reserve0 -= amount_out;
        } else {
            return Err(anyhow!("Token not in pool"));
        }
        self.last_updated = chrono::Utc::now().timestamp() as u64;
        Ok(())
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
        format!("v2-{}-{}-{}", self.address, self.token0, self.token1)
    }

    fn log_summary(&self) -> String {
        format!(
            "V2 Pool {} - {} <> {} (reserves: {}, {})",
            self.address, self.token0, self.token1, self.reserve0, self.reserve1
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

impl EventApplicable for UniswapV2Pool {
    fn apply_log(&mut self, log: &Log) -> Result<()> {
        match log.topic0() {
            Some(&IUniswapV2Pair::Sync::SIGNATURE_HASH) => {
                let sync_data: IUniswapV2Pair::Sync = log.log_decode()?.inner.data;
                debug!(
                    "Applying V2Sync event to pool {}: reserve0={}, reserve1={}",
                    self.address, sync_data.reserve0, sync_data.reserve1
                );
                self.update_reserves(
                    U256::from(sync_data.reserve0),
                    U256::from(sync_data.reserve1),
                )?;
                Ok(())
            }
            Some(&IV2PairUint256::Sync::SIGNATURE_HASH) => {
                let sync_data: IV2PairUint256::Sync = log.log_decode()?.inner.data;
                debug!(
                    "Applying V2Sync event to pool {}: reserve0={}, reserve1={}",
                    self.address, sync_data.reserve0, sync_data.reserve1
                );
                self.update_reserves(sync_data.reserve0, sync_data.reserve1)?;
                Ok(())
            }
            Some(&IUniswapV2Pair::Swap::SIGNATURE_HASH) => Ok(()),
            _ => {
                trace!("Ignoring unknown event for V2 pool");
                Ok(())
            }
        }
    }
}

impl TopicList for UniswapV2Pool {
    fn topics() -> Vec<FixedBytes<32>> {
        vec![
            IUniswapV2Pair::Swap::SIGNATURE_HASH,
            IUniswapV2Pair::Sync::SIGNATURE_HASH,
            IV2PairUint256::Sync::SIGNATURE_HASH,
        ]
    }

    fn profitable_topics() -> Vec<FixedBytes<32>> {
        vec![IUniswapV2Pair::Swap::SIGNATURE_HASH]
    }
}

impl fmt::Display for UniswapV2Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "V2 Pool {} - {} <> {} (reserves: {}, {})",
            self.address, self.token0, self.token1, self.reserve0, self.reserve1
        )
    }
}

impl PoolTypeTrait for UniswapV2Pool {
    fn pool_type(&self) -> PoolType {
        PoolType::UniswapV2
    }
}
